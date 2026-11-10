//! virtio-console (hvc0) without multiport: queue 0 receives (host -> guest), queue 1
//! transmits (guest -> host, to the VM's console output).
//!
//! Output buffers are returned to the driver right away: like the serial port, the
//! console drops output that its reader is not ready for (see [`ConsoleOutput`]).
//! Input waits until the driver has receive buffers for it (see [`start`]).

use std::collections::VecDeque;
use std::sync::Arc;

use async_executor::Executor;
use event_listener::Event;
use midgard_hv::GuestMemory;

use super::{Device, DeviceRef, Queue};
use crate::board::Board;
use crate::host::console::ConsoleOutput;
use crate::logging::log;

const NAME: &str = "virtio-console";
const DEVICE_ID: u32 = 3;
const RECEIVE_QUEUE: usize = 0;
const TRANSMIT_QUEUE: usize = 1;
const QUEUE_SIZE: u16 = 64;
/// The largest output buffer that we accept from the guest.
const MAX_OUTPUT: usize = 1 << 20;

pub(crate) struct Console {
    output: ConsoleOutput,
    /// Notified when the driver adds receive buffers.
    buffers: Arc<Event>,
}

impl Console {
    /// A console whose output goes to `output`.
    pub(crate) fn new(output: ConsoleOutput) -> Self {
        Self {
            output,
            buffers: Arc::default(),
        }
    }

    /// Moves input from `pending` into the driver's receive buffers, as far as it has
    /// any.
    fn receive(&mut self, pending: &mut VecDeque<u8>, queues: &mut [Queue], memory: &GuestMemory) {
        let queue = &mut queues[RECEIVE_QUEUE];
        while !pending.is_empty() {
            let Some(chain) = queue.next_chain(memory, NAME) else {
                break;
            };
            let count = chain.writable_len().min(pending.len());
            let data: Vec<u8> = pending.drain(..count).collect();
            let written = chain.write(memory, &data);
            queue.push(memory, chain.head, written);
        }
    }
}

/// Passes `input` to the console `device` as fast as the driver takes it (a task on
/// `executor`, which reaches the device through `board`).
pub(crate) fn start(
    executor: &Executor<'static>,
    board: Arc<Board>,
    device: DeviceRef<Console>,
    input: async_channel::Receiver<Vec<u8>>,
) {
    let buffers = board.with_device(device, |console, _, _| console.buffers.clone());
    executor
        .spawn(async move {
            // Kept across resets of the device (e.g., typing ahead while the guest boots).
            let mut pending = VecDeque::new();
            while let Ok(data) = input.recv().await {
                pending.extend(data);
                while !pending.is_empty() {
                    // Listen first, such that we do not miss new buffers.
                    let added = buffers.listen();
                    board.with_device(device, |console, queues, memory| {
                        console.receive(&mut pending, queues, memory)
                    });
                    if !pending.is_empty() {
                        added.await;
                    }
                }
            }
        })
        .detach();
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
        match queue {
            RECEIVE_QUEUE => {
                self.buffers.notify(1);
            }
            TRANSMIT_QUEUE => queues[TRANSMIT_QUEUE].process(memory, NAME, |chain| {
                match chain.read(memory, MAX_OUTPUT) {
                    Ok(data) => self.output.write(&data),
                    Err(error) => log!("{NAME}: {error}"),
                }
                0
            }),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_lite::future::{block_on, poll_once};

    use super::super::DESC_WRITE;
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

    /// Offers a receive buffer of `length` bytes as the chain `head` (at slot `head`).
    fn receive_buffer(memory: &GuestMemory, head: u16, length: u32) {
        let address = 0x4000 + head as u64 * 0x100;
        descriptor(memory, head, address, length, DESC_WRITE, 0);
        offer(memory, head, head);
    }

    fn received(memory: &GuestMemory, head: u16, length: usize) -> Vec<u8> {
        let mut data = vec![0u8; length];
        memory
            .read(0x4000 + head as u64 * 0x100, &mut data)
            .unwrap();
        data
    }

    #[test]
    fn input_fills_receive_buffers_as_they_come() {
        let mut console = Console::new(ConsoleOutput::new(piper::pipe(1).1));
        let memory = memory();
        let mut queues = [queue(), queue()];
        let mut pending: VecDeque<u8> = b"hello world".iter().copied().collect();
        // No buffers yet.
        console.receive(&mut pending, &mut queues, &memory);
        assert_eq!(pending.len(), 11);

        // New buffers wake the input task.
        let added = console.buffers.listen();
        receive_buffer(&memory, 0, 4);
        receive_buffer(&memory, 1, 4);
        console.process(RECEIVE_QUEUE, &mut queues, &memory);
        assert!(block_on(poll_once(added)).is_some());
        console.receive(&mut pending, &mut queues, &memory);
        assert!(take_pushed(&mut queues[RECEIVE_QUEUE]));
        assert_eq!(received(&memory, 0, 4), b"hell");
        assert_eq!(received(&memory, 1, 4), b"o wo");
        assert_eq!(pending, b"rld");
        // The used entries have the lengths of the input.
        assert_eq!(memory.read_volatile::<u32>(USED + 8), Ok(4));

        receive_buffer(&memory, 2, 64);
        console.receive(&mut pending, &mut queues, &memory);
        assert_eq!(received(&memory, 2, 3), b"rld");
        assert!(pending.is_empty());
        assert_eq!(memory.read_volatile::<u16>(USED + 2), Ok(3));
        assert_eq!(memory.read_volatile::<u32>(USED + 4 + 2 * 8 + 4), Ok(3));
    }
}
