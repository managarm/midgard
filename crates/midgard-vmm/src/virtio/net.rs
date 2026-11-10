//! virtio-net: the guest's Ethernet interface. Frames from the guest go to the network
//! stack ([`crate::host::net`]); the stack hands frames for the guest to [`VirtioNet::deliver`].

use std::collections::VecDeque;
use std::sync::Arc;

use midgard_hv::GuestMemory;
use zerocopy::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::{Device, Queue, read_config};
use crate::host::net;
use crate::logging::log;

const NAME: &str = "virtio-net";
const DEVICE_ID: u32 = 1;
const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;
const QUEUE_SIZE: u16 = 256;

const F_MAC: u64 = 1 << 5;

/// struct virtio_net_hdr_v1 (with VIRTIO_F_VERSION_1, it always has num_buffers).
#[derive(Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Header {
    flags: u8,
    gso_type: u8,
    hdr_len: U16,
    gso_size: U16,
    csum_start: U16,
    csum_offset: U16,
    num_buffers: U16,
}

const HEADER_SIZE: usize = size_of::<Header>();
const _: () = assert!(HEADER_SIZE == 12);
/// Frames for the guest that we keep while it has no receive buffers.
const MAX_PENDING: usize = 1024;
/// The largest packet that we accept from the guest (without offloads, frames are not
/// larger than the MTU).
const MAX_PACKET: usize = 64 * 1024;

pub(crate) struct VirtioNet {
    mac: [u8; 6],
    stack: Arc<net::UserNetHandle>,
    pending: VecDeque<Vec<u8>>,
    /// We reported that we dropped frames for the guest.
    reported_drop: bool,
}

impl VirtioNet {
    pub(crate) fn new(mac: [u8; 6], stack: Arc<net::UserNetHandle>) -> Self {
        Self {
            mac,
            stack,
            pending: VecDeque::new(),
            reported_drop: false,
        }
    }

    /// Queues frames for the guest.
    pub(crate) fn deliver(
        &mut self,
        frames: Vec<Vec<u8>>,
        queues: &mut [Queue],
        memory: &GuestMemory,
    ) {
        for frame in frames {
            if self.pending.len() < MAX_PENDING {
                self.pending.push_back(frame);
            } else if !self.reported_drop {
                // The guest does not refill its receive queue.
                log!("{NAME}: dropping frames for the guest (further drops are not reported)");
                self.reported_drop = true;
            }
        }
        self.fill(queues, memory)
    }

    fn fill(&mut self, queues: &mut [Queue], memory: &GuestMemory) {
        while let Some(frame) = self.pending.front() {
            let Some(chain) = queues[RX_QUEUE].next_chain(memory, NAME) else {
                break;
            };
            // No offloads; the frame is in one buffer.
            let header = Header {
                num_buffers: 1.into(),
                ..Default::default()
            };
            let mut packet = header.as_bytes().to_vec();
            packet.extend_from_slice(frame);
            let written = chain.write(memory, &packet);
            queues[RX_QUEUE].push(memory, chain.head, written);
            self.pending.pop_front();
        }
    }
}

impl Device for VirtioNet {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        F_MAC
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![QUEUE_SIZE, QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // struct virtio_net_config: the MAC address.
        read_config(&self.mac, offset, data);
    }

    fn process(&mut self, queue: usize, queues: &mut [Queue], memory: &GuestMemory) {
        if queue == TX_QUEUE {
            queues[TX_QUEUE].process(memory, NAME, |chain| {
                match chain.read(memory, MAX_PACKET) {
                    Ok(mut packet) if packet.len() > HEADER_SIZE => {
                        self.stack.send_from_guest(packet.split_off(HEADER_SIZE));
                    }
                    Ok(_) => {}
                    Err(error) => log!("{NAME}: {error}"),
                }
                0
            });
        }
        // New receive buffers (or room after sending).
        self.fill(queues, memory);
    }

    fn reset(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{USED, descriptor, memory, offer, queue, take_pushed};
    use super::super::{DESC_NEXT, DESC_WRITE};
    use super::*;

    #[test]
    fn frames_from_the_guest_go_to_the_stack() {
        let (handle, from_guest) = net::channel();
        let mut net = VirtioNet::new([0; 6], handle);
        let memory = memory();
        // The header and the frame in separate buffers.
        memory.write(0x4000, Header::default().as_bytes()).unwrap();
        memory.write(0x5000, b"frame").unwrap();
        descriptor(&memory, 0, 0x4000, HEADER_SIZE as u32, DESC_NEXT, 1);
        descriptor(&memory, 1, 0x5000, 5, 0, 0);
        offer(&memory, 0, 0);
        let mut queues = [queue(), queue()];

        net.process(TX_QUEUE, &mut queues, &memory);
        assert!(take_pushed(&mut queues[TX_QUEUE]));
        assert_eq!(from_guest.try_recv().unwrap(), b"frame");
    }

    #[test]
    fn frames_for_the_guest_wait_for_receive_buffers() {
        let (handle, _from_guest) = net::channel();
        let mut net = VirtioNet::new([0; 6], handle);
        let memory = memory();
        let mut queues = [queue(), queue()];

        // No receive buffers yet.
        net.deliver(vec![b"frame".to_vec()], &mut queues, &memory);
        assert!(!take_pushed(&mut queues[RX_QUEUE]));
        descriptor(&memory, 0, 0x4000, 1514, DESC_WRITE, 0);
        offer(&memory, 0, 0);
        net.process(RX_QUEUE, &mut queues, &memory);
        assert!(take_pushed(&mut queues[RX_QUEUE]));

        let mut packet = [0u8; HEADER_SIZE + 5];
        memory.read(0x4000, &mut packet).unwrap();
        let (header, frame) = Header::read_from_prefix(&packet).unwrap();
        assert_eq!(header.num_buffers.get(), 1);
        assert_eq!(frame, b"frame");
        // The used entry has the length of the header and the frame.
        let mut length = [0u8; 4];
        memory.read(USED + 8, &mut length).unwrap();
        assert_eq!(u32::from_le_bytes(length), packet.len() as u32);
    }

    #[test]
    fn frames_for_a_guest_that_takes_none_are_dropped() {
        let (handle, _from_guest) = net::channel();
        let mut net = VirtioNet::new([0; 6], handle);
        let memory = memory();
        let mut queues = [queue(), queue()];
        let frames = vec![b"frame".to_vec(); MAX_PENDING + 1];
        net.deliver(frames, &mut queues, &memory);
        assert!(!take_pushed(&mut queues[RX_QUEUE]));
        assert_eq!(net.pending.len(), MAX_PENDING);
        assert!(net.reported_drop);
    }
}
