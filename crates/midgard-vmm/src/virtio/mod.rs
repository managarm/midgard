//! virtio 1.x over the MMIO transport (version 2), with split virtqueues.
//! Linux finds the devices through `virtio_mmio.device=` on its command line.
//!
//! Each device implements [`Device`]; its [`Transport`] handles the registers, feature
//! negotiation and the queues' setup. The board routes MMIO accesses to the transports.
//! Devices with a host side (e.g., input that arrives from the host) reach their device
//! through [`DeviceRef`] (see [`crate::board::Board::with_device`]).

pub(crate) mod block;
pub(crate) mod vsock;

use std::marker::PhantomData;
use std::sync::atomic::{Ordering, fence};
use std::sync::{Arc, Condvar, Mutex};

use midgard_hv::GuestMemory;

use crate::logging::log;
use crate::platform;

/// The register window of one device.
pub(crate) const MMIO_SIZE: u64 = 0x1000;

const MAGIC: u32 = 0x7472_6976; // "virt"
const VERSION: u32 = 2;
const VENDOR: u32 = 0x4D47_524D; // "MRGM"

pub(crate) const F_VERSION_1: u64 = 1 << 32;
/// We support indirect descriptors for all devices.
const F_INDIRECT_DESC: u64 = 1 << 28;

// Device status bits.
const STATUS_FEATURES_OK: u32 = 8;

// Interrupt status bits.
const INTERRUPT_USED_BUFFER: u32 = 1;

// Register offsets.
const REG_MAGIC: u64 = 0x000;
const REG_VERSION: u64 = 0x004;
const REG_DEVICE_ID: u64 = 0x008;
const REG_VENDOR_ID: u64 = 0x00C;
const REG_DEVICE_FEATURES: u64 = 0x010;
const REG_DEVICE_FEATURES_SEL: u64 = 0x014;
const REG_DRIVER_FEATURES: u64 = 0x020;
const REG_DRIVER_FEATURES_SEL: u64 = 0x024;
const REG_QUEUE_SEL: u64 = 0x030;
const REG_QUEUE_NUM_MAX: u64 = 0x034;
const REG_QUEUE_NUM: u64 = 0x038;
const REG_QUEUE_READY: u64 = 0x044;
const REG_QUEUE_NOTIFY: u64 = 0x050;
const REG_INTERRUPT_STATUS: u64 = 0x060;
const REG_INTERRUPT_ACK: u64 = 0x064;
const REG_STATUS: u64 = 0x070;
const REG_QUEUE_DESC_LOW: u64 = 0x080;
const REG_QUEUE_DESC_HIGH: u64 = 0x084;
const REG_QUEUE_DRIVER_LOW: u64 = 0x090;
const REG_QUEUE_DRIVER_HIGH: u64 = 0x094;
const REG_QUEUE_DEVICE_LOW: u64 = 0x0A0;
const REG_QUEUE_DEVICE_HIGH: u64 = 0x0A4;
const REG_SHM_LEN_LOW: u64 = 0x0B0;
const REG_SHM_LEN_HIGH: u64 = 0x0B4;
const REG_SHM_BASE_LOW: u64 = 0x0B8;
const REG_SHM_BASE_HIGH: u64 = 0x0BC;
const REG_CONFIG_GENERATION: u64 = 0x0FC;
const REG_CONFIG: u64 = 0x100;

// Descriptor flags.
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const DESC_INDIRECT: u16 = 4;

/// The longest descriptor chain that we accept.
const MAX_CHAIN: usize = 4096;

/// A malformed request from the driver. The device reports it and skips the request.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("the {0} is outside of RAM")]
    OutsideRam(&'static str),
    #[error("bad descriptor chain (head {0})")]
    BadChain(u16),
    #[error("bad indirect descriptor (head {0})")]
    BadIndirect(u16),
    #[error("the available ring's index is {0} entries ahead (in a queue of {1})")]
    BadAvailIndex(u16, u16),
    #[error("a request of {0} bytes is too large")]
    TooLarge(u64),
    /// The request does not have the layout that the device expects.
    #[error("{0}")]
    Malformed(&'static str),
}

/// One buffer of a descriptor chain.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Descriptor {
    pub address: u64,
    pub length: u32,
    /// The device writes the buffer (otherwise, it reads it).
    pub writable: bool,
}

/// A descriptor chain that the driver made available.
pub(crate) struct Chain {
    pub head: u16,
    pub descriptors: Vec<Descriptor>,
}

impl Chain {
    pub(crate) fn readable(&self) -> impl Iterator<Item = &Descriptor> {
        self.descriptors.iter().filter(|d| !d.writable)
    }

    pub(crate) fn writable(&self) -> impl Iterator<Item = &Descriptor> {
        self.descriptors.iter().filter(|d| d.writable)
    }

    /// The size of the writable buffers.
    pub(crate) fn writable_len(&self) -> usize {
        self.writable().map(|d| d.length as usize).sum()
    }

    /// Reads the readable buffers (what the driver sends), which must not be larger than
    /// `max` bytes (the guest chooses their sizes).
    pub(crate) fn read(&self, memory: &GuestMemory, max: usize) -> Result<Vec<u8>, Error> {
        let total: u64 = self.readable().map(|d| d.length as u64).sum();
        if total > max as u64 {
            return Err(Error::TooLarge(total));
        }
        let mut bytes = vec![0u8; total as usize];
        let mut offset = 0;
        for descriptor in self.readable() {
            let end = offset + descriptor.length as usize;
            if memory
                .read(descriptor.address, &mut bytes[offset..end])
                .is_err()
            {
                return Err(Error::OutsideRam("buffer"));
            }
            offset = end;
        }
        Ok(bytes)
    }

    /// Writes `data` into the writable buffers; returns how many bytes fit.
    pub(crate) fn write(&self, memory: &GuestMemory, data: &[u8]) -> u32 {
        let mut written = 0;
        for descriptor in self.writable() {
            if written == data.len() {
                break;
            }
            let count = (descriptor.length as usize).min(data.len() - written);
            if memory
                .write(descriptor.address, &data[written..written + count])
                .is_err()
            {
                break;
            }
            written += count;
        }
        written as u32
    }
}

/// Copies the bytes at `offset` of a device's configuration space (`config`) into `data`;
/// bytes past its end read as zero.
pub(crate) fn read_config(config: &[u8], offset: u64, data: &mut [u8]) {
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = (offset as usize)
            .checked_add(i)
            .and_then(|index| config.get(index))
            .copied()
            .unwrap_or(0);
    }
}

/// What [`Queue::pop_one`] found.
enum Popped {
    Empty,
    Chain(Chain),
    /// A malformed chain (its head).
    Malformed(u16, Error),
}

/// A split virtqueue.
#[derive(Default)]
pub(crate) struct Queue {
    max_size: u16,
    size: u16,
    ready: bool,
    desc: u64,
    avail: u64,
    used: u64,
    next_avail: u16,
    next_used: u16,
    /// Chains were returned to the driver since the transport last looked (see
    /// [`Transport::signal_used`]).
    pushed: bool,
}

impl Queue {
    fn new(max_size: u16) -> Self {
        Self {
            max_size,
            size: max_size,
            ..Default::default()
        }
    }

    fn reset(&mut self) {
        *self = Self::new(self.max_size);
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready && self.size != 0
    }

    fn read_descriptor(
        &self,
        memory: &GuestMemory,
        table: u64,
        index: u16,
    ) -> Option<(u64, u32, u16, u16)> {
        let base = table.checked_add(index as u64 * 16)?;
        Some((
            memory.read_volatile::<u64>(base).ok()?,
            memory.read_volatile::<u32>(base.checked_add(8)?).ok()?,
            memory.read_volatile::<u16>(base.checked_add(12)?).ok()?,
            memory.read_volatile::<u16>(base.checked_add(14)?).ok()?,
        ))
    }

    /// Takes the next available descriptor chain. Malformed chains are reported (as
    /// errors of `device`) and returned to the driver unused; errors are problems with
    /// the ring itself.
    fn pop(&mut self, memory: &GuestMemory, device: &str) -> Result<Option<Chain>, Error> {
        loop {
            match self.pop_one(memory)? {
                Popped::Empty => return Ok(None),
                Popped::Chain(chain) => return Ok(Some(chain)),
                Popped::Malformed(head, error) => {
                    log!("{device}: {error}");
                    // Otherwise, the driver would wait for the request forever.
                    self.push(memory, head, 0);
                }
            }
        }
    }

    /// Like [`Queue::pop`], but returns malformed chains.
    fn pop_one(&mut self, memory: &GuestMemory) -> Result<Popped, Error> {
        if !self.is_ready() {
            return Ok(Popped::Empty);
        }
        let avail_idx = self
            .avail
            .checked_add(2)
            .and_then(|address| memory.read_volatile::<u16>(address).ok())
            .ok_or(Error::OutsideRam("avail ring"))?;
        let available = avail_idx.wrapping_sub(self.next_avail);
        if available == 0 {
            return Ok(Popped::Empty);
        }
        if available > self.size {
            return Err(Error::BadAvailIndex(available, self.size));
        }
        // Read the ring entry (and the descriptors) after the index.
        fence(Ordering::Acquire);
        let head = self
            .avail
            .checked_add(4 + (self.next_avail % self.size) as u64 * 2)
            .and_then(|address| memory.read_volatile::<u16>(address).ok())
            .ok_or(Error::OutsideRam("avail ring"))?;
        self.next_avail = self.next_avail.wrapping_add(1);
        Ok(match self.read_chain(memory, head) {
            Ok(descriptors) => Popped::Chain(Chain { head, descriptors }),
            Err(error) => Popped::Malformed(head, error),
        })
    }

    /// Reads the descriptors of the chain at `head`.
    fn read_chain(&self, memory: &GuestMemory, head: u16) -> Result<Vec<Descriptor>, Error> {
        let mut descriptors = Vec::new();
        let (mut table, mut table_size, mut index) = (self.desc, self.size, head);
        let mut indirect = false;
        loop {
            if index >= table_size || descriptors.len() > MAX_CHAIN {
                return Err(Error::BadChain(head));
            }
            let (address, length, flags, next) = self
                .read_descriptor(memory, table, index)
                .ok_or(Error::OutsideRam("descriptor table"))?;
            if flags & DESC_INDIRECT != 0 {
                if indirect || !length.is_multiple_of(16) {
                    return Err(Error::BadIndirect(head));
                }
                indirect = true;
                table = address;
                table_size = (length / 16).min(u16::MAX as u32) as u16;
                index = 0;
                continue;
            }
            descriptors.push(Descriptor {
                address,
                length,
                writable: flags & DESC_WRITE != 0,
            });
            if flags & DESC_NEXT == 0 {
                break;
            }
            index = next;
        }
        Ok(descriptors)
    }

    /// Like [`Queue::pop`], but reports problems with the ring (as errors of `device`)
    /// instead of returning them.
    pub(crate) fn next_chain(&mut self, memory: &GuestMemory, device: &str) -> Option<Chain> {
        self.pop(memory, device).unwrap_or_else(|error| {
            log!("{device}: {error}");
            None
        })
    }

    /// Hands each available chain to `handle`, which returns how many bytes it wrote
    /// into the chain, and returns the chains to the driver. Returns whether it used any.
    pub(crate) fn process(
        &mut self,
        memory: &GuestMemory,
        device: &str,
        mut handle: impl FnMut(&Chain) -> u32,
    ) -> bool {
        let mut used = false;
        while let Some(chain) = self.next_chain(memory, device) {
            let written = handle(&chain);
            self.push(memory, chain.head, written);
            used = true;
        }
        used
    }

    /// Returns a chain to the driver; `written` is the number of bytes that we wrote.
    pub(crate) fn push(&mut self, memory: &GuestMemory, head: u16, written: u32) {
        // E.g., a chain that the device kept across a reset.
        if !self.is_ready() {
            return;
        }
        // If the driver put the used ring outside of RAM (the addresses are the driver's
        // choice), it never sees the entry; like real DMA, that is its problem.
        if let Some(slot) = self
            .used
            .checked_add(4 + (self.next_used % self.size) as u64 * 8)
        {
            let _ = memory.write_volatile::<u32>(slot, head as u32);
            if let Some(length) = slot.checked_add(4) {
                let _ = memory.write_volatile::<u32>(length, written);
            }
        }
        self.next_used = self.next_used.wrapping_add(1);
        self.pushed = true;
        // The driver must see the entry before the index.
        fence(Ordering::Release);
        if let Some(index) = self.used.checked_add(2) {
            let _ = memory.write_volatile::<u16>(index, self.next_used);
        }
    }
}

/// A virtio device behind the transport.
pub(crate) trait Device: Send + std::any::Any {
    fn device_id(&self) -> u32;
    /// Device-specific feature bits (the transport adds VIRTIO_F_VERSION_1 and
    /// VIRTIO_F_INDIRECT_DESC).
    fn features(&self) -> u64 {
        0
    }
    /// The maximum size of each queue (which also determines the number of queues).
    fn queue_sizes(&self) -> Vec<u16>;
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    /// Called when the driver negotiated features (before it sets DRIVER_OK).
    fn set_driver_features(&mut self, _features: u64) {}
    /// Processes the available buffers of `queue`. Returns whether the device added
    /// used buffers (to any queue).
    fn process(&mut self, queue: usize, queues: &mut [Queue], memory: &GuestMemory) -> bool;
    fn reset(&mut self) {}
    /// The requests that the device's host side works on (e.g., I/O into the driver's
    /// buffers), if it does that outside of [`Device::process`]. A reset completes when
    /// they are done (see [`Effect::Reset`]).
    fn in_flight(&self) -> Option<InFlight> {
        None
    }
}

/// Counts the requests of a device that its host side works on (see
/// [`Device::in_flight`]).
#[derive(Clone, Default)]
pub(crate) struct InFlight(Arc<Counter>);

#[derive(Default)]
struct Counter {
    count: Mutex<usize>,
    /// Notified when the count drops to zero.
    idle: Condvar,
}

impl InFlight {
    /// Counts a request until the returned token is dropped (also if, e.g., its task
    /// panics).
    pub(crate) fn start(&self) -> Pending {
        *self.0.count.lock().unwrap() += 1;
        Pending(self.clone())
    }

    /// Blocks until no requests are in flight.
    pub(crate) fn wait(&self) {
        let mut count = self.0.count.lock().unwrap();
        while *count > 0 {
            count = self.0.idle.wait(count).unwrap();
        }
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        *self.0.count.lock().unwrap()
    }
}

/// A request that [`InFlight`] counts until this is dropped.
pub(crate) struct Pending(InFlight);

impl Drop for Pending {
    fn drop(&mut self) {
        let mut count = self
            .0
            .0
            .count
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *count -= 1;
        if *count == 0 {
            self.0.0.idle.notify_all();
        }
    }
}

/// What the board does after a register write (see [`Transport::write`]).
pub(crate) enum Effect {
    None,
    /// Raise the device's interrupt.
    Interrupt,
    /// The driver reset the device, whose host side may still work on requests (e.g.,
    /// I/O into the driver's buffers). With virtio-mmio version 2, the reset is complete
    /// when the write is, so the write must wait for them (without the device's lock,
    /// which their completion takes) and then call [`Transport::finish_reset`].
    Reset(InFlight),
}

/// A handle to a VM's virtio device of type `D` (see [`crate::board::Board::with_device`]).
pub(crate) struct DeviceRef<D> {
    slot: usize,
    device: PhantomData<fn() -> D>,
}

impl<D> Clone for DeviceRef<D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D> Copy for DeviceRef<D> {}

impl<D> DeviceRef<D> {
    /// The device's slot (see [`platform::virtio_slot`]).
    pub(crate) fn slot(self) -> usize {
        self.slot
    }
}

/// The virtio devices of a VM, in the order of their slots.
#[derive(Default)]
pub(crate) struct DeviceList(Vec<Box<dyn Device>>);

impl DeviceList {
    pub(crate) fn add<D: Device>(&mut self, device: D) -> DeviceRef<D> {
        self.0.push(Box::new(device));
        DeviceRef {
            slot: self.0.len() - 1,
            device: PhantomData,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// The devices' transports, with their interrupt lines.
    pub(crate) fn into_transports(self) -> Vec<Transport> {
        self.0
            .into_iter()
            .enumerate()
            .map(|(slot, device)| Transport::new(device, platform::virtio_slot(slot).1))
            .collect()
    }
}

/// The MMIO transport of one device.
pub(crate) struct Transport {
    device: Box<dyn Device>,
    queues: Vec<Queue>,
    pub irq: usize,
    status: u32,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    queue_sel: u32,
    interrupt_status: u32,
    /// The driver reset the device, whose requests are not done yet (see
    /// [`Effect::Reset`]).
    resetting: bool,
}

impl Transport {
    pub(crate) fn new(device: Box<dyn Device>, irq: usize) -> Self {
        let queues = device.queue_sizes().into_iter().map(Queue::new).collect();
        Self {
            device,
            queues,
            irq,
            status: 0,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            queue_sel: 0,
            interrupt_status: 0,
            resetting: false,
        }
    }

    fn device_features(&self) -> u64 {
        self.device.features() | F_VERSION_1 | F_INDIRECT_DESC
    }

    fn reset(&mut self) {
        for queue in &mut self.queues {
            queue.reset();
        }
        self.device.reset();
        self.status = 0;
        self.driver_features = 0;
        self.interrupt_status = 0;
        self.queue_sel = 0;
    }

    fn selected(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// Sets the used buffer interrupt if the device `added` used buffers (or any queue
    /// returned chains, e.g., malformed ones); returns whether to raise the interrupt
    /// line. (We ignore VRING_AVAIL_F_NO_INTERRUPT; spurious interrupts are harmless.)
    fn signal_used(&mut self, added: bool) -> bool {
        let pushed = self.queues.iter_mut().fold(false, |pushed, queue| {
            std::mem::take(&mut queue.pushed) | pushed
        });
        if added || pushed {
            self.interrupt_status |= INTERRUPT_USED_BUFFER;
        }
        added || pushed
    }

    pub(crate) fn read(&mut self, offset: u64, size: u8) -> u64 {
        if offset >= REG_CONFIG {
            let mut data = [0u8; 8];
            let size = (size as usize).min(8);
            self.device
                .read_config(offset - REG_CONFIG, &mut data[..size]);
            return u64::from_le_bytes(data);
        }
        let features = self.device_features();
        let value = match offset {
            REG_MAGIC => MAGIC,
            REG_VERSION => VERSION,
            REG_DEVICE_ID => self.device.device_id(),
            REG_VENDOR_ID => VENDOR,
            REG_DEVICE_FEATURES => match self.device_features_sel {
                0 => features as u32,
                1 => (features >> 32) as u32,
                _ => 0,
            },
            REG_QUEUE_NUM_MAX => self.selected().map_or(0, |q| q.max_size as u32),
            REG_QUEUE_READY => self.selected().map_or(0, |q| q.ready as u32),
            REG_INTERRUPT_STATUS => self.interrupt_status,
            REG_STATUS => self.status,
            // No shared memory regions: a length of all ones means "absent".
            REG_SHM_LEN_LOW | REG_SHM_LEN_HIGH => u32::MAX,
            REG_SHM_BASE_LOW | REG_SHM_BASE_HIGH => u32::MAX,
            // The configuration never changes.
            REG_CONFIG_GENERATION => 0,
            _ => 0,
        };
        value as u64
    }

    pub(crate) fn write(
        &mut self,
        offset: u64,
        size: u8,
        value: u64,
        memory: &GuestMemory,
    ) -> Effect {
        // The driver must wait for the reset (see finish_reset()).
        if self.resetting {
            return Effect::None;
        }
        if offset >= REG_CONFIG {
            let bytes = value.to_le_bytes();
            self.device
                .write_config(offset - REG_CONFIG, &bytes[..(size as usize).min(8)]);
            return Effect::None;
        }
        let value = value as u32;
        let set_low = |old: u64, value: u32| (old & !0xFFFF_FFFF) | value as u64;
        let set_high = |old: u64, value: u32| (old & 0xFFFF_FFFF) | (value as u64) << 32;
        match offset {
            REG_DEVICE_FEATURES_SEL => self.device_features_sel = value,
            REG_DRIVER_FEATURES => {
                let shift = match self.driver_features_sel {
                    0 => 0,
                    1 => 32,
                    _ => return Effect::None,
                };
                self.driver_features =
                    (self.driver_features & !(0xFFFF_FFFFu64 << shift)) | ((value as u64) << shift);
            }
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            REG_QUEUE_SEL => self.queue_sel = value,
            REG_QUEUE_NUM => {
                if let Some(queue) = self.selected()
                    && value <= queue.max_size as u32
                    && value.is_power_of_two()
                {
                    queue.size = value as u16;
                }
            }
            REG_QUEUE_READY => {
                if let Some(queue) = self.selected() {
                    queue.ready = value & 1 != 0;
                }
            }
            REG_QUEUE_DESC_LOW => self
                .selected()
                .into_iter()
                .for_each(|q| q.desc = set_low(q.desc, value)),
            REG_QUEUE_DESC_HIGH => self
                .selected()
                .into_iter()
                .for_each(|q| q.desc = set_high(q.desc, value)),
            REG_QUEUE_DRIVER_LOW => self
                .selected()
                .into_iter()
                .for_each(|q| q.avail = set_low(q.avail, value)),
            REG_QUEUE_DRIVER_HIGH => self
                .selected()
                .into_iter()
                .for_each(|q| q.avail = set_high(q.avail, value)),
            REG_QUEUE_DEVICE_LOW => self
                .selected()
                .into_iter()
                .for_each(|q| q.used = set_low(q.used, value)),
            REG_QUEUE_DEVICE_HIGH => self
                .selected()
                .into_iter()
                .for_each(|q| q.used = set_high(q.used, value)),
            REG_QUEUE_NOTIFY => {
                let queue = value as usize & 0xFFFF;
                if queue < self.queues.len() && self.queues[queue].is_ready() {
                    let added = self.device.process(queue, &mut self.queues, memory);
                    if self.signal_used(added) {
                        return Effect::Interrupt;
                    }
                }
            }
            REG_INTERRUPT_ACK => self.interrupt_status &= !value,
            REG_STATUS => {
                if value == 0 {
                    let status = self.status;
                    self.reset();
                    if let Some(in_flight) = self.device.in_flight() {
                        // Until the requests are done, the status shows (e.g., to other
                        // vCPUs) that the reset is not complete.
                        self.status = status;
                        self.resetting = true;
                        return Effect::Reset(in_flight);
                    }
                } else {
                    if value & STATUS_FEATURES_OK != 0 && self.status & STATUS_FEATURES_OK == 0 {
                        // Only features that we offered can be accepted.
                        let accepted = self.driver_features & self.device_features();
                        if accepted != self.driver_features {
                            // Leave FEATURES_OK unset; the driver gives up.
                            self.status = value & !STATUS_FEATURES_OK;
                            return Effect::None;
                        }
                        self.device.set_driver_features(accepted);
                    }
                    self.status = value;
                }
            }
            _ => {}
        }
        Effect::None
    }

    /// Completes a reset after the device's requests are done (see [`Effect::Reset`]).
    pub(crate) fn finish_reset(&mut self) {
        self.resetting = false;
        self.status = 0;
    }

    /// Runs `f` on the device, which is a `D` (e.g., for host-side requests). `f` returns
    /// whether it added used buffers; the result is whether to raise the interrupt.
    pub(crate) fn with_device<D: Device, R>(
        &mut self,
        memory: &GuestMemory,
        f: impl FnOnce(&mut D, &mut [Queue], &GuestMemory) -> (R, bool),
    ) -> (R, bool) {
        let device = (self.device.as_mut() as &mut dyn std::any::Any)
            .downcast_mut::<D>()
            .expect("DeviceRef of the wrong device type");
        let (result, added) = f(device, &mut self.queues, memory);
        (result, self.signal_used(added))
    }
}

/// Helpers for the devices' tests: a queue at fixed addresses, and memory with room for
/// buffers from 0x4000.
#[cfg(test)]
mod tests {
    use super::*;

    const DESC: u64 = 0x1000;
    const AVAIL: u64 = 0x2000;
    pub(super) const USED: u64 = 0x3000;
    const SIZE: u16 = 8;

    pub(super) fn memory() -> GuestMemory {
        GuestMemory::detached(0x10000)
    }

    pub(super) fn queue() -> Queue {
        Queue {
            max_size: SIZE,
            size: SIZE,
            ready: true,
            desc: DESC,
            avail: AVAIL,
            used: USED,
            next_avail: 0,
            next_used: 0,
            pushed: false,
        }
    }

    pub(super) fn descriptor(
        memory: &GuestMemory,
        index: u16,
        address: u64,
        length: u32,
        flags: u16,
        next: u16,
    ) {
        let base = DESC + index as u64 * 16;
        memory.write_u64(base, address).unwrap();
        memory.write(base + 8, &length.to_le_bytes()).unwrap();
        memory.write(base + 12, &flags.to_le_bytes()).unwrap();
        memory.write(base + 14, &next.to_le_bytes()).unwrap();
    }

    /// Makes the chain at `head` available.
    pub(super) fn offer(memory: &GuestMemory, slot: u16, head: u16) {
        memory
            .write(AVAIL + 4 + slot as u64 * 2, &head.to_le_bytes())
            .unwrap();
        memory.write(AVAIL + 2, &(slot + 1).to_le_bytes()).unwrap();
    }

    #[test]
    fn read_and_write_chains() {
        let memory = memory();
        // Two readable buffers, then two writable ones.
        memory.write(0x4000, b"hello ").unwrap();
        memory.write(0x5000, b"world").unwrap();
        descriptor(&memory, 0, 0x4000, 6, DESC_NEXT, 1);
        descriptor(&memory, 1, 0x5000, 5, DESC_NEXT, 2);
        descriptor(&memory, 2, 0x6000, 3, DESC_NEXT | DESC_WRITE, 3);
        descriptor(&memory, 3, 0x7000, 100, DESC_WRITE, 0);
        offer(&memory, 0, 0);

        let mut queue = queue();
        let chain = queue.next_chain(&memory, "test").unwrap();
        assert_eq!(chain.read(&memory, 64).unwrap(), b"hello world");
        assert!(matches!(chain.read(&memory, 10), Err(Error::TooLarge(11))));
        assert_eq!(chain.writable_len(), 103);
        assert_eq!(chain.write(&memory, b"abcdef"), 6);
        let mut bytes = [0u8; 3];
        memory.read(0x7000, &mut bytes).unwrap();
        assert_eq!(&bytes, b"def");
        queue.push(&memory, chain.head, 6);
        assert_eq!(memory.read_volatile::<u16>(USED + 2), Ok(1));
        assert!(queue.next_chain(&memory, "test").is_none());
    }

    #[test]
    fn indirect_and_bad_chains() {
        let memory = memory();
        // An indirect table with two descriptors.
        let table = 0x8000;
        memory.write_u64(table, 0x4000).unwrap();
        memory.write(table + 8, &4u32.to_le_bytes()).unwrap();
        memory.write(table + 12, &DESC_NEXT.to_le_bytes()).unwrap();
        memory.write(table + 14, &1u16.to_le_bytes()).unwrap();
        memory.write_u64(table + 16, 0x5000).unwrap();
        memory.write(table + 24, &8u32.to_le_bytes()).unwrap();
        memory.write(table + 28, &DESC_WRITE.to_le_bytes()).unwrap();
        descriptor(&memory, 0, table, 32, DESC_INDIRECT, 0);
        // A chain that loops forever, then a good one.
        descriptor(&memory, 1, 0x4000, 1, DESC_NEXT, 1);
        descriptor(&memory, 2, 0x4000, 1, 0, 0);
        offer(&memory, 0, 0);
        offer(&memory, 1, 1);
        offer(&memory, 2, 2);

        let mut queue = queue();
        let chain = queue.pop(&memory, "test").unwrap().unwrap();
        assert_eq!(chain.descriptors.len(), 2);
        assert_eq!(chain.writable_len(), 8);
        // The bad chain goes back to the driver, and the next one is not held up.
        assert!(!queue.pushed);
        let chain = queue.pop(&memory, "test").unwrap().unwrap();
        assert_eq!(chain.head, 2);
        assert!(queue.pushed);
        assert_eq!(memory.read_volatile::<u16>(USED + 2), Ok(1));
        assert_eq!(memory.read_volatile::<u32>(USED + 4), Ok(1));
        // The driver claims more available chains than the queue holds.
        memory
            .write(AVAIL + 2, &(3 + SIZE + 1).to_le_bytes())
            .unwrap();
        assert!(matches!(
            queue.pop(&memory, "test"),
            Err(Error::BadAvailIndex(9, SIZE))
        ));
    }

    #[test]
    fn rings_at_the_end_of_the_address_space() {
        let memory = memory();
        let mut queue = queue();
        queue.avail = u64::MAX - 1;
        assert!(matches!(
            queue.pop(&memory, "test"),
            Err(Error::OutsideRam("avail ring"))
        ));
        queue.used = u64::MAX - 1;
        queue.push(&memory, 0, 0);

        // An indirect table at the end of the address space.
        let mut queue = self::queue();
        descriptor(&memory, 0, u64::MAX - 15, 32, DESC_INDIRECT, 0);
        offer(&memory, 0, 0);
        assert!(matches!(
            queue.pop_one(&memory),
            Ok(Popped::Malformed(0, Error::OutsideRam("descriptor table")))
        ));
    }

    #[test]
    fn in_flight() {
        let in_flight = InFlight::default();
        in_flight.wait();
        let requests = [in_flight.start(), in_flight.start()];
        let thread = std::thread::spawn(move || drop(requests));
        in_flight.wait();
        assert_eq!(in_flight.count(), 0);
        thread.join().unwrap();
    }

    /// A device whose host side works on one request.
    struct Busy(InFlight);

    impl Device for Busy {
        fn device_id(&self) -> u32 {
            0
        }

        fn queue_sizes(&self) -> Vec<u16> {
            vec![SIZE]
        }

        fn read_config(&self, _offset: u64, _data: &mut [u8]) {}

        fn process(&mut self, _queue: usize, _queues: &mut [Queue], _memory: &GuestMemory) -> bool {
            false
        }

        fn in_flight(&self) -> Option<InFlight> {
            Some(self.0.clone())
        }
    }

    #[test]
    fn reset_waits_for_requests() {
        let memory = memory();
        let in_flight = InFlight::default();
        let request = in_flight.start();
        let mut transport = Transport::new(Box::new(Busy(in_flight.clone())), 5);
        transport.write(REG_STATUS, 4, 1, &memory);
        let Effect::Reset(wait) = transport.write(REG_STATUS, 4, 0, &memory) else {
            panic!("the reset does not wait");
        };
        // The reset is not complete, and the driver cannot use the device.
        assert_eq!(transport.read(REG_STATUS, 4), 1);
        transport.write(REG_STATUS, 4, 3, &memory);
        assert_eq!(transport.read(REG_STATUS, 4), 1);
        assert_eq!(wait.count(), 1);
        drop(request);
        wait.wait();
        transport.finish_reset();
        assert_eq!(transport.read(REG_STATUS, 4), 0);
    }

    #[test]
    fn config_space() {
        let mut data = [0xFFu8; 4];
        read_config(&[1, 2, 3], 1, &mut data);
        assert_eq!(data, [2, 3, 0, 0]);
        read_config(&[1, 2, 3], u64::MAX, &mut data);
        assert_eq!(data, [0; 4]);
    }
}
