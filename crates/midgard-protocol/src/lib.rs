//! The interface between bifrost (the CLI), midgard (the VMM daemon) and the guest
//! agent (midgard-init): bifrost decides how a VM looks ([`machine::MachineConfig`]) and
//! asks midgard to run it (see [`control`]) on midgard's socket (see [`runtime`]);
//! bifrost and the agent speak the session protocol (see [`session`]); midgard tells the
//! agent how to set up the guest (see [`guest`]). It also has the constants that both
//! sides of the VM need to agree on.
//!
//! Each message is a frame: a little-endian u32 length and the message in postcard.
//! bifrost, midgard and the agent come from the same installation, so the encoding does
//! not need to be self-describing.

#![warn(unreachable_pub)]

use std::io::{self, Read, Write};

use serde::{Serialize, de::DeserializeOwned};

pub mod control;
pub mod guest;
pub mod machine;
pub mod runtime;
pub mod session;

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
    let payload = postcard::to_stdvec(message).map_err(invalid)?;
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
    postcard::from_bytes(&payload).map(Some).map_err(invalid)
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
        postcard::from_bytes(&payload).map(Some).map_err(invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest::{Display, GuestConfig, Launch, Launcher, Ready};
    use crate::machine::{ConsoleKind, Disk, MachineConfig};
    use crate::session::{Exec, Request, Response, WindowSize};

    #[test]
    fn machine_config_round_trip() {
        let config = MachineConfig {
            kernel: "/boot/bzImage".into(),
            initrd: None,
            agent: Some("/usr/lib/midgard/midgard-init".into()),
            cmdline: None,
            append: vec!["quiet".into(), "loglevel=3".into()],
            memory: 2 << 30,
            cpus: 4,
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
            control::Response::Ready,
            control::Response::Running(vec!["alpine".into()]),
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
        round_trip_request(Request::Ping);
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
        round_trip_response(Response::Pong);
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
