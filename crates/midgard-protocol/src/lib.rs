//! The interface between bifrost (the CLI), midgard (the VMM daemon) and the guest
//! agent (midgard-init): bifrost decides how a VM looks ([`machine::MachineConfig`]) and
//! asks midgard to run it (see [`control`]) on midgard's socket (see [`runtime`]);
//! bifrost and the agent speak the session protocol (see [`session`]); midgard tells the
//! agent how to set up the guest (see [`guest`]). It also has the constants that both
//! sides of the VM need to agree on, and how sizes are written (see [`size`]).
//!
//! Each message is a frame: a little-endian u32 length and the message in MessagePack,
//! with structs as maps of their fields' names and enums by their variants' names
//! (`rmp_serde::to_vec_named`). The encoding describes itself because the agent is built
//! and distributed apart from midgard: a peer skips the fields that it does not know and
//! uses the defaults of those that it misses, such that messages can grow; what else
//! changes between midgard and the agent has a version (see [`version`]).

#![warn(unreachable_pub)]

use std::io::{self, Read, Write};

use serde::{Serialize, de::DeserializeOwned};

pub mod control;
pub mod guest;
pub mod machine;
pub mod runtime;
pub mod session;
pub mod size;
pub mod version;

/// The vsock CID of the host (midgard itself).
pub const HOST_CID: u32 = 2;
/// The vsock CID of the guest.
pub const GUEST_CID: u32 = 3;

/// The vsock port that the agent listens on.
pub const AGENT_PORT: u32 = 1024;

/// The host vsock port that the guest's waypipe connects to (once per Wayland client).
pub const WAYPIPE_PORT: u32 = 1025;

/// The virtio-fs tag of the host's shared directory.
pub const SHARE_TAG: &str = "managarm";
/// Where the agent mounts the shared directory.
pub const SHARE_MOUNT: &str = "/mnt/managarm";

/// Kernel parameters that midgard passes to the agent.
pub mod param {
    /// The root file system's device.
    pub const ROOT: &str = "midgard.root";
    /// The root file system's type (default: ext4).
    pub const ROOT_FSTYPE: &str = "midgard.rootfstype";
}

/// Frames larger than this are rejected.
pub const MAX_FRAME: usize = 1 << 20;

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

/// Encodes a message as a frame.
fn encode<T: Serialize>(message: &T) -> io::Result<Vec<u8>> {
    let payload = rmp_serde::to_vec_named(message).map_err(invalid)?;
    if payload.len() > MAX_FRAME {
        return Err(invalid("message is too large"));
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// The payload length of a frame with this header.
fn payload_length(header: [u8; 4]) -> io::Result<usize> {
    let length = u32::from_le_bytes(header) as usize;
    if length > MAX_FRAME {
        return Err(invalid("frame is too large"));
    }
    Ok(length)
}

/// Writes one message.
pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    writer.write_all(&encode(message)?)?;
    writer.flush()
}

/// Reads one message; returns None at the end of the stream (between messages).
pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<Option<T>> {
    let mut header = [0u8; 4];
    let mut done = 0;
    while done < 4 {
        match reader.read(&mut header[done..])? {
            0 if done == 0 => return Ok(None),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            count => done += count,
        }
    }
    let mut payload = vec![0u8; payload_length(header)?];
    reader.read_exact(&mut payload)?;
    rmp_serde::from_slice(&payload).map(Some).map_err(invalid)
}

/// The same for async streams (the `async` feature).
#[cfg(feature = "async")]
pub mod nonblocking {
    use futures_lite::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    use super::*;

    /// Writes one message.
    pub async fn write_message<T: Serialize>(
        writer: &mut (impl AsyncWrite + Unpin),
        message: &T,
    ) -> io::Result<()> {
        writer.write_all(&encode(message)?).await?;
        writer.flush().await
    }

    /// Reads one message; returns None at the end of the stream (between messages).
    pub async fn read_message<T: DeserializeOwned>(
        reader: &mut (impl AsyncRead + Unpin),
    ) -> io::Result<Option<T>> {
        let mut header = [0u8; 4];
        let mut done = 0;
        while done < 4 {
            match reader.read(&mut header[done..]).await? {
                0 if done == 0 => return Ok(None),
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                count => done += count,
            }
        }
        let mut payload = vec![0u8; payload_length(header)?];
        reader.read_exact(&mut payload).await?;
        rmp_serde::from_slice(&payload).map(Some).map_err(invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest::{Display, GuestConfig, Launch, Launcher, Ready};
    use crate::machine::{ConsoleKind, Disk, MachineConfig};
    use crate::session::{Exec, Request, Response, WindowSize};
    use crate::version::{Hello, Versions};

    #[test]
    fn machine_config_round_trip() {
        let config = MachineConfig {
            kernel: "/boot/bzImage".into(),
            initrd: None,
            agent: Some("/usr/lib/midgard/midgard-init".into()),
            cmdline: None,
            append: vec!["quiet".into(), "loglevel=3".into()],
            memory: 2 << 30,
            cpus: Some(4),
            console: ConsoleKind::Virtio,
            disks: vec![Disk {
                path: "/var/lib/midgard/alpine.qcow2".into(),
                read_only: false,
            }],
            network: true,
            share: Some("/".into()),
            guest: GuestConfig {
                init: Some("/sbin/init".into()),
                ready: Ready::Systemd,
                user: Some("alice".into()),
                user_setup: Some(vec!["/etc/midgard/user-setup".into()]),
                launcher: Launcher::Wrapper(vec!["runuser".into(), "{helper}".into()]),
                display: Display::Systemd,
                waypipe_debug: true,
            },
            waypipe: Some("/usr/lib/midgard/guest-waypipe".into()),
        };
        let mut stream = Vec::new();
        write_message(&mut stream, &config).unwrap();
        let mut reader = stream.as_slice();
        assert_eq!(read_message(&mut reader).unwrap(), Some(config));
        assert_eq!(read_message::<MachineConfig>(&mut reader).unwrap(), None);
    }

    #[test]
    fn control_messages() {
        let mut stream = Vec::new();
        let responses = [
            control::Response::Console(b"Linux version".to_vec()),
            control::Response::Stopped("the guest powered off".into()),
            control::Response::Starting,
            control::Response::Ready { protocol: 1 },
            control::Response::NoInstance,
            control::Response::Running(vec!["alpine".into()]),
            control::Response::List(control::Listing {
                instances: vec![control::InstanceInfo {
                    name: "dev".into(),
                    template: Some("debian".into()),
                    running: true,
                    disk_size: Some(1 << 30),
                    error: None,
                }],
                templates: vec![control::TemplateInfo {
                    name: "debian".into(),
                    description: None,
                    needs_user: true,
                    error: Some("no disk".into()),
                }],
                templates_dir: "/var/lib/midgard/templates".into(),
            }),
            control::Response::Pull(control::PullEvent::Downloading {
                file: "alpine.tar".into(),
                done: 1 << 20,
                total: 1 << 30,
            }),
            control::Response::Pull(control::PullEvent::Installed(control::PullItem::BootFiles(
                "linux".into(),
            ))),
            control::Response::Pulled(control::Pulled {
                installed: vec!["alpine".into()],
                available: vec!["debian".into()],
            }),
            control::Response::Done,
            control::Response::Error("no such file".into()),
        ];
        for response in &responses {
            write_message(&mut stream, response).unwrap();
        }
        let mut reader = stream.as_slice();
        for response in responses {
            assert_eq!(read_message(&mut reader).unwrap(), Some(response));
        }
    }

    #[test]
    fn control_requests() {
        let mut stream = Vec::new();
        let requests = [
            control::Request::Session {
                name: "alpine".into(),
                start: true,
            },
            control::Request::Input(b"ls\r".to_vec()),
            control::Request::Create {
                name: "dev".into(),
                template: "debian".into(),
                user: Some("alice".into()),
                memory: Some(2 << 30),
            },
            control::Request::Delete { name: "dev".into() },
            control::Request::Pull(control::Pull {
                templates: vec!["alpine".into()],
                all: false,
                from: Some("/srv/guests".into()),
            }),
        ];
        for request in &requests {
            write_message(&mut stream, request).unwrap();
        }
        let mut reader = stream.as_slice();
        for request in requests {
            assert_eq!(read_message(&mut reader).unwrap(), Some(request));
        }
    }

    #[test]
    fn truncated_frames() {
        let mut stream = Vec::new();
        write_message(&mut stream, &42u32).unwrap();
        for length in 1..stream.len() {
            let error = read_message::<u32>(&mut &stream[..length]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        }
    }

    fn round_trip_request(request: Request) {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &request).unwrap();
        let mut reader = &buffer[..];
        assert_eq!(read_message(&mut reader).unwrap(), Some(request));
        assert_eq!(read_message::<Request>(&mut reader).unwrap(), None);
    }

    fn round_trip_response(response: Response) {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &response).unwrap();
        assert_eq!(read_message(&mut &buffer[..]).unwrap(), Some(response));
    }

    #[test]
    fn session_requests() {
        round_trip_request(Request::Exec(Exec {
            argv: vec!["ls".into(), "-l".into()],
            env: vec![("TERM".into(), "xterm".into())],
            cwd: Some("/root".into()),
            user: Some("alice".into()),
            tty: Some(WindowSize { rows: 24, cols: 80 }),
        }));
        round_trip_request(Request::Stdin(b"hello".to_vec()));
        round_trip_request(Request::StdinEof);
        round_trip_request(Request::Resize(WindowSize { rows: 1, cols: 2 }));
        round_trip_request(Request::Signal(15));
        round_trip_request(Request::Shutdown);
        round_trip_request(Request::Hello(Hello::OURS));
    }

    #[test]
    fn session_responses() {
        round_trip_response(Response::Started { pid: 42 });
        round_trip_response(Response::Stdout(vec![0, 1, 2]));
        round_trip_response(Response::Exited {
            code: None,
            signal: Some(9),
        });
        round_trip_response(Response::Error("no such file".into()));
        round_trip_response(Response::Hello(Hello::OURS));
    }

    #[test]
    fn launch_round_trip() {
        let launch = Launch {
            argv: vec!["ls".into()],
            env: vec![("A".into(), "b".into())],
            cwd: "/home/alice".into(),
        };
        let mut buffer = Vec::new();
        write_message(&mut buffer, &launch).unwrap();
        assert_eq!(read_message(&mut &buffer[..]).unwrap(), Some(launch));
    }

    /// The encoding that other builds of the agent rely on.
    #[test]
    fn wire_format() {
        let encode = |request: &Request| rmp_serde::to_vec_named(request).unwrap();
        // A unit variant is its name.
        assert_eq!(encode(&Request::StdinEof), b"\xa8StdinEof");
        // Other variants are maps from their name to their value; bytes are binary.
        assert_eq!(
            encode(&Request::Stdin(vec![1, 200])),
            b"\x81\xa5Stdin\xc4\x02\x01\xc8"
        );
        assert_eq!(encode(&Request::Signal(15)), b"\x81\xa6Signal\x0f");
        // Structs are maps from their fields' names.
        let exited = Response::Exited {
            code: None,
            signal: Some(9),
        };
        assert_eq!(
            rmp_serde::to_vec_named(&exited).unwrap(),
            b"\x81\xa6Exited\x82\xa4code\xc0\xa6signal\x09"
        );
        let mut frame = Vec::new();
        write_message(&mut frame, &Request::StdinEof).unwrap();
        assert_eq!(frame, b"\x09\0\0\0\xa8StdinEof");
        // What midgard and the agent say first.
        let hello = Hello {
            protocol: Versions {
                oldest: 1,
                current: 2,
            },
        };
        assert_eq!(
            encode(&Request::Hello(hello)),
            b"\x81\xa5Hello\x81\xa8protocol\x82\xa6oldest\x01\xa7current\x02"
        );
        assert_eq!(
            rmp_serde::to_vec_named(&Response::Hello(hello)).unwrap(),
            b"\x81\xa5Hello\x81\xa8protocol\x82\xa6oldest\x01\xa7current\x02"
        );
    }

    /// A peer of another build sends fields that we do not know and lacks some of ours.
    #[test]
    fn tolerates_other_fields() {
        #[derive(Serialize)]
        struct NewerConfig {
            init: Option<String>,
            ready: Ready,
            swap: Vec<u64>,
            hostname: Option<String>,
        }
        let newer = NewerConfig {
            init: Some("/sbin/init".into()),
            ready: Ready::Notify,
            swap: vec![1, 2],
            hostname: Some("guest".into()),
        };
        let bytes = rmp_serde::to_vec_named(&newer).unwrap();
        let config: GuestConfig = rmp_serde::from_slice(&bytes).unwrap();
        let expected = GuestConfig {
            init: Some("/sbin/init".into()),
            ready: Ready::Notify,
            ..GuestConfig::default()
        };
        assert_eq!(config, expected);

        // An empty map.
        let config: GuestConfig = rmp_serde::from_slice(b"\x80").unwrap();
        assert_eq!(config, GuestConfig::default());

        #[derive(Serialize)]
        struct OlderExec {
            argv: Vec<String>,
        }
        #[derive(Serialize)]
        enum OlderRequest {
            Exec(OlderExec),
        }
        let older = OlderRequest::Exec(OlderExec {
            argv: vec!["ls".into()],
        });
        let bytes = rmp_serde::to_vec_named(&older).unwrap();
        let expected = Request::Exec(Exec {
            argv: vec!["ls".into()],
            ..Exec::default()
        });
        assert_eq!(rmp_serde::from_slice::<Request>(&bytes).unwrap(), expected);
    }

    #[test]
    fn config_file() {
        use crate::guest::{ConfigError, encode_config, read_config};
        use crate::version::{Mismatch, PROTOCOL};

        let config = GuestConfig {
            init: Some("/sbin/init".into()),
            ..GuestConfig::default()
        };
        let file = encode_config(&config).unwrap();
        let (version, read) = read_config(&mut file.as_slice()).unwrap();
        assert_eq!((version, read), (PROTOCOL, config));

        // A midgard that is too new for us; we do not look at its configuration.
        let newer = Hello {
            protocol: Versions {
                oldest: PROTOCOL + 1,
                current: PROTOCOL + 2,
            },
        };
        let mut file = Vec::new();
        write_message(&mut file, &newer).unwrap();
        file.extend_from_slice(b"\x01\0\0\0\xc1");
        let error = read_config(&mut file.as_slice()).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::Mismatch(Mismatch::PeerTooNew { .. })
        ));
        assert!(matches!(
            read_config(&mut &[][..]).unwrap_err(),
            ConfigError::Io(_)
        ));
    }

    #[test]
    fn rejects_garbage() {
        // A frame whose payload is not a request.
        assert!(read_message::<Request>(&mut &[1u8, 0, 0, 0, 99][..]).is_err());
        // A truncated frame.
        assert!(read_message::<Request>(&mut &[8u8, 0, 0, 0, 0][..]).is_err());
        // Too large.
        assert!(read_message::<Request>(&mut &[0u8, 0, 0, 0x7F][..]).is_err());
    }
}
