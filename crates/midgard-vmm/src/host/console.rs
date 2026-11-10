//! The VM's console output: the devices write into the pipe that the caller of
//! [`crate::start`] reads from.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::logging::log;

/// The writing side of the console's pipe, shared by the devices. Output that does not
/// fit into the pipe is dropped: the guest does not wait for the reader (Linux writes to
/// its console with interrupts disabled, so waiting would stall the guest's kernel).
#[derive(Clone)]
pub(crate) struct ConsoleOutput(Arc<Shared>);

struct Shared {
    writer: Mutex<piper::Writer>,
    /// We reported that we dropped output.
    reported_drop: AtomicBool,
}

impl ConsoleOutput {
    pub(crate) fn new(writer: piper::Writer) -> Self {
        Self(Arc::new(Shared {
            writer: Mutex::new(writer),
            reported_drop: AtomicBool::new(false),
        }))
    }

    /// Writes `data` (as much as fits; without a reader, nothing).
    pub(crate) fn write(&self, data: &[u8]) {
        let mut writer = self.0.writer.lock().unwrap();
        let mut rest = data;
        // The writer only sees what the reader took when it believes that the pipe is
        // full: a short write is followed by another one that finds out.
        while !rest.is_empty() {
            match writer.try_fill(rest) {
                0 => break,
                written => rest = &rest[written..],
            }
        }
        if !rest.is_empty()
            && !writer.is_closed()
            && !self.0.reported_drop.swap(true, Ordering::Relaxed)
        {
            log!("dropping console output (its reader is behind; further drops are not reported)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_that_does_not_fit_is_dropped() {
        let (mut reader, writer) = piper::pipe(4);
        let output = ConsoleOutput::new(writer);
        output.clone().write(b"ab");
        output.write(b"cdef");
        let mut data = [0u8; 8];
        assert_eq!(reader.try_drain(&mut data), 4);
        assert_eq!(&data[..4], b"abcd");
        assert!(output.0.reported_drop.load(Ordering::Relaxed));
        // There is room again.
        output.write(b"g");
        assert_eq!(reader.try_drain(&mut data), 1);
        assert_eq!(data[0], b'g');
    }

    #[test]
    fn output_fits_once_the_reader_took_it() {
        let (mut reader, writer) = piper::pipe(8);
        let output = ConsoleOutput::new(writer);
        let mut data = [0u8; 8];
        output.write(b"abcde");
        assert_eq!(reader.try_drain(&mut data), 5);
        // The writer still believes that the reader did not take "abcde".
        output.write(b"fghij");
        assert_eq!(reader.try_drain(&mut data), 5);
        assert_eq!(&data[..5], b"fghij");
        assert!(!output.0.reported_drop.load(Ordering::Relaxed));
    }

    #[test]
    fn output_without_a_reader_is_dropped_silently() {
        let (reader, writer) = piper::pipe(4);
        drop(reader);
        let output = ConsoleOutput::new(writer);
        output.write(b"lost");
        assert!(!output.0.reported_drop.load(Ordering::Relaxed));
    }
}
