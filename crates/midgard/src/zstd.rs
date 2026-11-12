//! Reads what zstd compressed: one or more frames (as `zstd -T0` and pzstd write), which may
//! be interleaved with skippable frames (which pzstd writes) and whose checksums we check.

use std::io::{self, Chain, Cursor, Read};
use std::mem;

use ruzstd::decoding::{FrameDecoder, StreamingDecoder};

/// The magic number at the start of a frame.
const MAGIC: u32 = 0xfd2f_b528;
/// The magic numbers of skippable frames, without the lowest four bits.
const SKIPPABLE_MAGIC: u32 = 0x184d_2a50;

/// Whether `data` starts like what zstd compressed.
pub fn is_compressed(data: &[u8]) -> bool {
    let Some(magic) = data.first_chunk::<4>() else {
        return false;
    };
    let magic = u32::from_le_bytes(*magic);
    magic == MAGIC || magic & !0xf == SKIPPABLE_MAGIC
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn truncated() -> io::Error {
    invalid("the zstd stream is truncated".into())
}

/// A frame, which we start reading after its magic number.
type Frame<R> = StreamingDecoder<Chain<Cursor<[u8; 4]>, R>, FrameDecoder>;

enum State<R: Read> {
    /// Before a frame (or at the end).
    Between(R),
    Frame(Box<Frame<R>>),
    /// After an error.
    Broken,
}

/// Decompresses what `R` reads.
pub struct Frames<R: Read> {
    state: State<R>,
}

impl<R: Read> Frames<R> {
    pub fn new(input: R) -> Self {
        Self {
            state: State::Between(input),
        }
    }
}

/// Reads all of `buffer` unless the input ends first; returns how much it read.
fn read_up_to(input: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut done = 0;
    while done < buffer.len() {
        match input.read(&mut buffer[done..]) {
            Ok(0) => break,
            Ok(count) => done += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(done)
}

impl<R: Read> Frames<R> {
    /// Reads from the frame that we are in, or starts the next one; returns 0 at the end.
    fn step(&mut self, buffer: &mut [u8]) -> io::Result<Option<usize>> {
        match mem::replace(&mut self.state, State::Broken) {
            State::Broken => Err(io::Error::other(
                "an earlier part of the zstd stream is invalid",
            )),
            State::Frame(mut frame) => {
                let count = frame.read(buffer)?;
                if count > 0 {
                    self.state = State::Frame(frame);
                    return Ok(Some(count));
                }
                let decoder = &frame.decoder;
                if let Some(expected) = decoder.get_checksum_from_data()
                    && decoder.get_calculated_checksum() != Some(expected)
                {
                    return Err(invalid("the checksum of a zstd frame is wrong".into()));
                }
                let (_, input) = frame.into_inner().into_inner();
                self.state = State::Between(input);
                Ok(None)
            }
            State::Between(mut input) => {
                let mut magic = [0; 4];
                match read_up_to(&mut input, &mut magic)? {
                    0 => {
                        self.state = State::Between(input);
                        return Ok(Some(0));
                    }
                    4 => {}
                    _ => return Err(truncated()),
                }
                let number = u32::from_le_bytes(magic);
                if number & !0xf == SKIPPABLE_MAGIC {
                    let mut length = [0; 4];
                    if read_up_to(&mut input, &mut length)? != 4 {
                        return Err(truncated());
                    }
                    let length = u64::from(u32::from_le_bytes(length));
                    if io::copy(&mut (&mut input).take(length), &mut io::sink())? != length {
                        return Err(truncated());
                    }
                    self.state = State::Between(input);
                    return Ok(None);
                }
                let frame = StreamingDecoder::new(Cursor::new(magic).chain(input))
                    .map_err(|error| invalid(format!("invalid zstd frame: {error}")))?;
                self.state = State::Frame(Box::new(frame));
                Ok(None)
            }
        }
    }
}

impl<R: Read> Read for Frames<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(count) = self.step(buffer)? {
                return Ok(count);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ruzstd::encoding::{CompressionLevel, compress_to_vec};

    use super::*;

    /// `printf hello | zstd --check`.
    const HELLO: [u8; 18] = [
        0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x58, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o', 0xa3,
        0x6d, 0x9f, 0x88,
    ];

    fn skippable(data: &[u8]) -> Vec<u8> {
        let mut frame = (SKIPPABLE_MAGIC | 0xa).to_le_bytes().to_vec();
        frame.extend((data.len() as u32).to_le_bytes());
        frame.extend(data);
        frame
    }

    fn decompress(data: &[u8]) -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        Frames::new(data).read_to_end(&mut output)?;
        Ok(output)
    }

    #[test]
    fn reads_frames() {
        assert_eq!(decompress(&HELLO).unwrap(), b"hello");
        let mut data = skippable(b"pzstd");
        data.extend(compress_to_vec(&b"one "[..], CompressionLevel::Fastest));
        data.extend(skippable(b""));
        data.extend(HELLO);
        assert!(is_compressed(&data));
        assert!(is_compressed(&HELLO));
        assert!(!is_compressed(b"ustar"));
        assert_eq!(decompress(&data).unwrap(), b"one hello");
        assert_eq!(decompress(b"").unwrap(), b"");
    }

    #[test]
    fn rejects() {
        let mut checksum = HELLO;
        checksum[17] ^= 1;
        let error = decompress(&checksum).unwrap_err();
        assert!(error.to_string().contains("checksum"), "{error}");
        // Truncated in a frame, in a skippable frame and in a magic number.
        assert!(decompress(&HELLO[..12]).is_err());
        assert!(decompress(&skippable(b"pzstd")[..10]).is_err());
        let mut data = HELLO.to_vec();
        data.extend(&HELLO[..2]);
        assert!(decompress(&data).is_err());
        assert!(decompress(&[0x28, 0xb5, 0x2f, 0xfd, 0xff, 0xff]).is_err());
    }
}
