//! virtio-console (hvc0) without multiport: queue 0 receives (host -> guest), queue 1
//! transmits (guest -> host, to the VM's console output).
//!
//! Output buffers are returned to the driver right away: like the serial port, the
//! console drops output that its reader is not ready for (see [`ConsoleOutput`]).

use midgard_hv::GuestMemory;

use super::{Device, Queue};
use crate::host::console::ConsoleOutput;
use crate::logging::log;

const NAME: &str = "virtio-console";
const DEVICE_ID: u32 = 3;
const TRANSMIT_QUEUE: usize = 1;
const QUEUE_SIZE: u16 = 64;
/// The largest output buffer that we accept from the guest.
const MAX_OUTPUT: usize = 1 << 20;

pub(crate) struct Console {
    output: ConsoleOutput,
}

impl Console {
    /// A console whose output goes to `output`.
    pub(crate) fn new(output: ConsoleOutput) -> Self {
        Self { output }
    }
}

impl Device for Console {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![QUEUE_SIZE, QUEUE_SIZE]
    }

    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        // Without features, the configuration space is unused.
        data.fill(0);
    }

    fn process(&mut self, queue: usize, queues: &mut [Queue], memory: &GuestMemory) {
        if queue == TRANSMIT_QUEUE {
            queues[TRANSMIT_QUEUE].process(memory, NAME, |chain| {
                match chain.read(memory, MAX_OUTPUT) {
                    Ok(data) => self.output.write(&data),
                    Err(error) => log!("{NAME}: {error}"),
                }
                0
            });
        }
        // Receive buffers wait for input.
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{USED, descriptor, memory, offer, queue, take_pushed};
    use super::*;

    /// Makes `data` available on the transmit queue as the chain `head` (at slot
    /// `head`).
    fn write(memory: &GuestMemory, head: u16, data: &[u8]) {
        let address = 0x4000 + head as u64 * 0x100;
        memory.write(address, data).unwrap();
        descriptor(memory, head, address, data.len() as u32, 0, 0);
        offer(memory, head, head);
    }

    #[test]
    fn output_goes_to_the_console() {
        let (mut reader, writer) = piper::pipe(8);
        let mut console = Console::new(ConsoleOutput::new(writer));
        let memory = memory();
        let mut queues = [queue(), queue()];
        write(&memory, 0, b"hello");
        write(&memory, 1, b" world");
        console.process(TRANSMIT_QUEUE, &mut queues, &memory);
        // Both chains are returned, although only part of the second one fits.
        assert!(take_pushed(&mut queues[TRANSMIT_QUEUE]));
        assert_eq!(memory.read_volatile::<u16>(USED + 2), Ok(2));
        let mut data = [0u8; 16];
        let count = reader.try_drain(&mut data);
        assert_eq!(&data[..count], b"hello wo");
    }
}
