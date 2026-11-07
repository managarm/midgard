//! The interface between bifrost (the CLI) and midgard (the VMM daemon): bifrost decides
//! how a VM looks ([`machine::MachineConfig`]) and asks midgard to run it (see [`control`]) on
//! midgard's socket (see [`runtime`]).
//!
//! Each message is a frame: a little-endian u32 length and the message in postcard.
//! bifrost and midgard come from the same installation, so the encoding does not need
//! to be self-describing.

#![warn(unreachable_pub)]

use std::io::{self, Read, Write};

use serde::{Serialize, de::DeserializeOwned};

pub mod control;
pub mod machine;
pub mod runtime;

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
    use crate::machine::{Disk, MachineConfig};

    #[test]
    fn machine_config_round_trip() {
        let config = MachineConfig {
            kernel: "/boot/bzImage".into(),
            initrd: Some("/boot/initrd".into()),
            cmdline: None,
            append: vec!["quiet".into(), "loglevel=3".into()],
            memory: 2 << 30,
            cpus: 4,
            disks: vec![Disk {
                path: "/var/lib/midgard/alpine.qcow2".into(),
                read_only: false,
            }],
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
    fn truncated_frames() {
        let mut stream = Vec::new();
        write_message(&mut stream, &42u32).unwrap();
        for length in 1..stream.len() {
            let error = read_message::<u32>(&mut &stream[..length]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        }
    }
}
