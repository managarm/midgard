//! The virtio-pci transport (without the legacy interface) with MSI-X.
//!
//! Each device is a PCI function with a single 32-bit memory BAR of [`BAR_SIZE`] bytes.
//! Vendor-specific capabilities tell the driver where in it the transport's structures
//! are, and the MSI-X capability where the MSI-X table and its pending bit array are:
//! - the common configuration (the [`Register`]s),
//! - the ISR status (which a read clears),
//! - the device's configuration space,
//! - the notification area, where each queue has an address of its own
//!   (`queue_notify_off`, see [`Register::QueueNotifyOff`]),
//! - the MSI-X table, with a vector for each queue and one for configuration changes.
//!
//! The function has no INTx pin, so the driver must use MSI-X to get interrupts.
//!
//! There is no VIRTIO_PCI_CAP_PCI_CFG capability (the window into the BAR through the
//! configuration space), which the specification requires: only firmware uses it, and
//! the guest boots without firmware.

use super::{Effect, Half, Register, Transport};
use crate::counters::Counter;
use crate::pci::{self, ConfigSpace};

/// The vendor ID of virtio devices; their device ID is [`DEVICE_ID_BASE`] plus the virtio
/// device ID.
const VENDOR: u16 = 0x1AF4;
const DEVICE_ID_BASE: u16 = 0x1040;
/// The subsystem ID of QEMU's devices without the legacy interface (the specification
/// leaves it to the environment).
const SUBSYSTEM_ID: u16 = 0x1100;
/// Devices without the legacy interface have revision 1 (or above).
const REVISION: u8 = 1;

// The layout of the BAR.
pub(crate) const BAR_SIZE: u64 = 0x8000;
const COMMON: u64 = 0x0000;
/// The common configuration up to queue_device (no queue_notify_data or queue_reset,
/// which need features that we do not offer).
const COMMON_LEN: u64 = 0x38;
const ISR: u64 = 0x1000;
const DEVICE: u64 = 0x2000;
const DEVICE_LEN: u64 = 0x1000;
const NOTIFY: u64 = 0x3000;
/// The distance between the queues' notification addresses.
const NOTIFY_MULTIPLIER: u64 = 4;
const MSIX_TABLE: u64 = 0x4000;
const MSIX_PBA: u64 = 0x6000;

/// The fields of the common configuration: their offset, size and register.
const COMMON_FIELDS: [(u64, u8, Register); 19] = [
    (0x00, 4, Register::DeviceFeaturesSel),
    (0x04, 4, Register::DeviceFeatures),
    (0x08, 4, Register::DriverFeaturesSel),
    (0x0C, 4, Register::DriverFeatures),
    (0x10, 2, Register::ConfigVector),
    (0x12, 2, Register::NumQueues),
    (0x14, 1, Register::Status),
    (0x15, 1, Register::ConfigGeneration),
    (0x16, 2, Register::QueueSel),
    (0x18, 2, Register::QueueSize),
    (0x1A, 2, Register::QueueVector),
    (0x1C, 2, Register::QueueReady),
    (0x1E, 2, Register::QueueNotifyOff),
    (0x20, 4, Register::QueueDesc(Half::Low)),
    (0x24, 4, Register::QueueDesc(Half::High)),
    (0x28, 4, Register::QueueDriver(Half::Low)),
    (0x2C, 4, Register::QueueDriver(Half::High)),
    (0x30, 4, Register::QueueDevice(Half::Low)),
    (0x34, 4, Register::QueueDevice(Half::High)),
];

// The vendor-specific capability and its structure types.
const CAP_VENDOR: u8 = 0x09;
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_ISR: u8 = 3;
const CFG_DEVICE: u8 = 4;
/// The length of the capability (struct virtio_pci_cap), and of the notification
/// structure's (which adds the multiplier).
const CAP_LEN: u8 = 16;
const NOTIFY_CAP_LEN: u8 = 20;

// MSI-X.
const MSIX_ENTRY_SIZE: u64 = 16;
/// Vector Control: the vector is masked.
const MSIX_VECTOR_MASKED: u32 = 1;
const MSIX_CONTROL_ENABLE: u16 = 1 << 15;
const MSIX_CONTROL_FUNCTION_MASK: u16 = 1 << 14;

/// The PCI class of a virtio device (by its virtio device ID).
fn class(device_id: u32) -> u32 {
    match device_id {
        // Network: Ethernet controller.
        1 => 0x02_0000,
        // Block and file system: other mass storage controller.
        2 | 26 => 0x01_8000,
        // Console and vsock: other communication controller.
        3 | 19 => 0x07_8000,
        _ => 0xFF_0000,
    }
}

/// The low `size` bytes of `value`.
fn truncate(value: u64, size: u8) -> u64 {
    if size >= 8 {
        value
    } else {
        value & ((1 << (8 * size)) - 1)
    }
}

/// A capability of the virtio structure `cfg_type` at `offset` of the BAR (after the
/// capability ID and the next pointer).
fn virtio_capability(cfg_type: u8, offset: u64, length: u64) -> Vec<u8> {
    // cap_len, cfg_type, bar, id, padding.
    let mut body = vec![CAP_LEN, cfg_type, 0, 0, 0, 0];
    body.extend((offset as u32).to_le_bytes());
    body.extend((length as u32).to_le_bytes());
    body
}

impl Transport {
    fn num_queues(&self) -> u64 {
        self.queue_vectors.len() as u64
    }

    /// The device's PCI function with its BAR at `bar`.
    pub(crate) fn config_space(&self, bar: u32) -> ConfigSpace {
        let device_id = DEVICE_ID_BASE + self.device_id() as u16;
        let mut space = ConfigSpace::new(VENDOR, device_id, class(self.device_id()), REVISION);
        space.set_subsystem(VENDOR, SUBSYSTEM_ID);
        space.add_memory_bar(0, bar, BAR_SIZE as u32);
        space.add_capability(
            CAP_VENDOR,
            &virtio_capability(CFG_COMMON, COMMON, COMMON_LEN),
        );
        let notify_len = self.num_queues().max(1) * NOTIFY_MULTIPLIER;
        let mut notify = virtio_capability(CFG_NOTIFY, NOTIFY, notify_len);
        notify[0] = NOTIFY_CAP_LEN;
        notify.extend((NOTIFY_MULTIPLIER as u32).to_le_bytes());
        space.add_capability(CAP_VENDOR, &notify);
        space.add_capability(CAP_VENDOR, &virtio_capability(CFG_ISR, ISR, 1));
        space.add_capability(
            CAP_VENDOR,
            &virtio_capability(CFG_DEVICE, DEVICE, DEVICE_LEN),
        );
        // Message Control (the table size minus one), then the table's and the pending
        // bit array's offsets (in BAR 0).
        let vectors = self.num_queues() as u16 + 1;
        let mut msix = (vectors - 1).to_le_bytes().to_vec();
        msix.extend((MSIX_TABLE as u32).to_le_bytes());
        msix.extend((MSIX_PBA as u32).to_le_bytes());
        space.add_capability(pci::CAP_MSIX, &msix);
        space
    }

    /// Reads `size` bytes of the BAR at `offset`.
    pub(crate) fn bar_read(&self, offset: u64, size: u8) -> u64 {
        self.counters.registers.reads.add();
        match offset {
            COMMON..ISR => self.read_common(offset - COMMON, size),
            ISR => self.take_interrupt_status().into(),
            DEVICE..NOTIFY => self.read_config(offset - DEVICE, size),
            MSIX_TABLE..MSIX_PBA => {
                (self.msix.lock().unwrap()).read_table(offset - MSIX_TABLE, size)
            }
            MSIX_PBA..BAR_SIZE => self.msix.lock().unwrap().read_pba(offset - MSIX_PBA, size),
            _ => 0,
        }
    }

    /// Writes `size` bytes of the BAR at `offset`; `send` sends MSIs that were pending
    /// (if the write unmasks their vector).
    pub(crate) fn bar_write(
        &self,
        offset: u64,
        size: u8,
        value: u64,
        send: impl Fn(u64, u32),
    ) -> Effect {
        self.counters.registers.writes.add();
        match offset {
            COMMON..ISR => return self.write_common(offset - COMMON, size, value),
            DEVICE..NOTIFY => self.write_config(offset - DEVICE, size, value),
            NOTIFY..MSIX_TABLE => self.notify(((offset - NOTIFY) / NOTIFY_MULTIPLIER) as u32),
            MSIX_TABLE..MSIX_PBA => {
                let mut msix = self.msix.lock().unwrap();
                msix.write_table(offset - MSIX_TABLE, size, value, &send);
            }
            _ => {}
        }
        Effect::None
    }

    /// The driver changed the MSI-X capability's Message Control register.
    pub(crate) fn set_msix_control(&self, control: u16, send: impl Fn(u64, u32)) {
        self.msix.lock().unwrap().set_control(control, &send);
    }

    fn read_common(&self, offset: u64, size: u8) -> u64 {
        // A 64-bit field in one access.
        if size == 8 {
            return self.read_common(offset, 4) | self.read_common(offset + 4, 4) << 32;
        }
        let field = COMMON_FIELDS
            .iter()
            .find(|(start, len, _)| (*start..start + u64::from(*len)).contains(&offset));
        let Some(&(start, _, register)) = field else {
            return 0;
        };
        truncate(
            u64::from(self.read_register(register)) >> (8 * (offset - start)),
            size,
        )
    }

    /// Writes a field of the common configuration; partial writes are ignored.
    fn write_common(&self, offset: u64, size: u8, value: u64) -> Effect {
        // A 64-bit field in one access.
        if size == 8 {
            let _ = self.write_common(offset, 4, value & 0xFFFF_FFFF);
            return self.write_common(offset + 4, 4, value >> 32);
        }
        let field = COMMON_FIELDS
            .iter()
            .find(|&&(start, len, _)| start == offset && len == size);
        match field {
            Some(&(_, _, register)) => self.write_register(register, value as u32),
            None => Effect::None,
        }
    }
}

/// The MSI-X table and its pending bits.
pub(super) struct Msix {
    /// The enable bit of Message Control (without it, the function would use INTx).
    enabled: bool,
    /// The function mask bit of Message Control.
    function_masked: bool,
    /// For each vector: the message address (low, high), the message data and the vector
    /// control.
    table: Vec<[u32; 4]>,
    pending: Vec<bool>,
    /// The messages that were sent (for statistics).
    sent: Counter,
}

impl Msix {
    /// A table with `vectors` vectors, which are masked (like after a reset).
    pub(super) fn new(vectors: usize) -> Self {
        Self {
            enabled: false,
            function_masked: false,
            table: vec![[0, 0, 0, MSIX_VECTOR_MASKED]; vectors],
            pending: vec![false; vectors],
            sent: Counter::default(),
        }
    }

    fn masked(&self, vector: usize) -> bool {
        !self.enabled || self.function_masked || self.table[vector][3] & MSIX_VECTOR_MASKED != 0
    }

    fn send(&self, vector: usize, send: &dyn Fn(u64, u32)) {
        self.sent.add();
        let [low, high, data, _] = self.table[vector];
        send(u64::from(low) | u64::from(high) << 32, data);
    }

    /// The messages that were sent.
    pub(super) fn sent(&self) -> u64 {
        self.sent.get()
    }

    /// Sends the vector's message, or makes it pending while the vector is masked.
    pub(super) fn signal(&mut self, vector: u16, send: &dyn Fn(u64, u32)) {
        let vector = usize::from(vector);
        if vector >= self.table.len() {
            return;
        }
        if self.masked(vector) {
            self.pending[vector] = true;
        } else {
            self.send(vector, send);
        }
    }

    /// Sends the messages of the pending vectors that are not masked.
    fn deliver_pending(&mut self, send: &dyn Fn(u64, u32)) {
        for vector in 0..self.table.len() {
            if self.pending[vector] && !self.masked(vector) {
                self.pending[vector] = false;
                self.send(vector, send);
            }
        }
    }

    fn set_control(&mut self, control: u16, send: &dyn Fn(u64, u32)) {
        self.enabled = control & MSIX_CONTROL_ENABLE != 0;
        self.function_masked = control & MSIX_CONTROL_FUNCTION_MASK != 0;
        self.deliver_pending(send);
    }

    /// The table's 32-bit word at `offset` (zero past its end).
    fn table_word(&self, offset: u64) -> u32 {
        let (vector, word) = (
            (offset / MSIX_ENTRY_SIZE) as usize,
            (offset % MSIX_ENTRY_SIZE / 4) as usize,
        );
        self.table.get(vector).map_or(0, |entry| entry[word])
    }

    /// Reads the table (in 32-bit words; narrower reads see part of one).
    fn read_table(&self, offset: u64, size: u8) -> u64 {
        let aligned = offset & !3;
        let value =
            u64::from(self.table_word(aligned)) | u64::from(self.table_word(aligned + 4)) << 32;
        truncate(value >> (8 * (offset - aligned)), size)
    }

    /// Writes the table (32-bit or 64-bit aligned writes; others are ignored, their
    /// effect is undefined).
    fn write_table(&mut self, offset: u64, size: u8, value: u64, send: &dyn Fn(u64, u32)) {
        if !(size == 4 || size == 8) || !offset.is_multiple_of(u64::from(size)) {
            return;
        }
        for i in 0..u64::from(size / 4) {
            let offset = offset + 4 * i;
            let (vector, word) = (
                (offset / MSIX_ENTRY_SIZE) as usize,
                (offset % MSIX_ENTRY_SIZE / 4) as usize,
            );
            let Some(entry) = self.table.get_mut(vector) else {
                continue;
            };
            let value = (value >> (32 * i)) as u32;
            // Of the vector control, only the mask bit exists.
            entry[word] = if word == 3 {
                value & MSIX_VECTOR_MASKED
            } else {
                value
            };
        }
        // E.g., the driver unmasked a vector.
        self.deliver_pending(send);
    }

    /// Reads the pending bit array (in 64-bit words).
    fn read_pba(&self, offset: u64, size: u8) -> u64 {
        let aligned = offset & !7;
        let first = (aligned * 8) as usize;
        let word = (0..64)
            .filter(|bit| self.pending.get(first + bit).copied().unwrap_or(false))
            .fold(0u64, |word, bit| word | 1 << bit);
        truncate(word >> (8 * (offset - aligned)), size)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;
    use crate::virtio::tests::memory;
    use crate::virtio::{Device, Queue};
    use midgard_hv::GuestMemory;

    /// A device with two queues and four bytes of configuration space.
    struct Dummy;

    impl Device for Dummy {
        fn device_id(&self) -> u32 {
            2
        }

        fn queue_sizes(&self) -> Vec<u16> {
            vec![8, 16]
        }

        fn read_config(&self, offset: u64, data: &mut [u8]) {
            crate::virtio::read_config(&[1, 2, 3, 4], offset, data);
        }

        fn process(&mut self, queue: usize, queues: &mut [Queue], memory: &GuestMemory) {
            // Returns a chain on each notification.
            queues[queue].push(memory, 0, 0);
        }
    }

    /// The function's configuration space as a PCI bus shows it.
    fn bus(transport: &Transport) -> pci::Bus {
        let mut bus = pci::Bus::new();
        bus.add(1, transport.config_space(0xE000_0000));
        bus
    }

    /// The registers of slot 1 in the bus' ECAM region.
    fn config(bus: &pci::Bus, register: u64, size: u8) -> u64 {
        bus.config_read(1 << 15 | register, size)
    }

    #[test]
    fn capabilities() {
        let transport = Transport::new(Box::new(Dummy));
        let bus = bus(&transport);
        assert_eq!(config(&bus, 0, 4), 0x1042_1AF4);
        assert_eq!(config(&bus, 0x08, 4), 0x0180_0001);
        assert_eq!(config(&bus, 0x2C, 4), 0x1100_1AF4);
        assert_eq!(config(&bus, 0x10, 4), 0xE000_0000);
        assert_eq!(bus.find_bar(0xE000_7FFF), Some((1, 0, 0x7FFF)));
        // No INTx pin.
        assert_eq!(config(&bus, 0x3D, 1), 0);
        // Walk the capabilities: (cfg_type, offset, length) of the virtio structures, and
        // the MSI-X capability.
        let mut structures = Vec::new();
        let mut msix = None;
        let mut capability = config(&bus, 0x34, 1);
        while capability != 0 {
            match config(&bus, capability, 1) as u8 {
                CAP_VENDOR => structures.push((
                    config(&bus, capability + 3, 1),
                    config(&bus, capability + 8, 4),
                    config(&bus, capability + 12, 4),
                )),
                pci::CAP_MSIX => msix = Some(capability),
                id => panic!("unexpected capability {id:#x}"),
            }
            capability = config(&bus, capability + 1, 1);
        }
        assert_eq!(
            structures,
            [
                (1, 0, 0x38),
                (2, 0x3000, 8),
                (3, 0x1000, 1),
                (4, 0x2000, 0x1000)
            ]
        );
        let msix = msix.unwrap();
        // Three vectors (two queues and configuration changes), the table and the pending
        // bit array in BAR 0.
        assert_eq!(config(&bus, msix + 2, 2), 2);
        assert_eq!(config(&bus, msix + 4, 4), 0x4000);
        assert_eq!(config(&bus, msix + 8, 4), 0x6000);
        // The notification multiplier.
        assert_eq!(config(&bus, 0x40 + 16 + 16, 4), 4);
    }

    #[test]
    fn common_configuration() {
        let transport = Transport::new(Box::new(Dummy));
        assert_eq!(transport.bar_read(0x12, 2), 2);
        // VIRTIO_F_VERSION_1 is in the second word of the features.
        transport.bar_write(0x00, 4, 1, |_, _| {});
        assert_eq!(transport.bar_read(0x04, 4) & 1, 1);
        // The second queue: its maximum size, notification offset and addresses.
        transport.bar_write(0x16, 2, 1, |_, _| {});
        assert_eq!(transport.bar_read(0x18, 2), 16);
        assert_eq!(transport.bar_read(0x1E, 2), 1);
        transport.bar_write(0x18, 2, 4, |_, _| {});
        assert_eq!(transport.bar_read(0x18, 2), 4);
        transport.bar_write(0x20, 4, 0x1000, |_, _| {});
        transport.bar_write(0x24, 4, 0x2, |_, _| {});
        transport.bar_write(0x28, 8, 0x3_0000_2000, |_, _| {});
        assert_eq!(transport.bar_read(0x20, 8), 0x2_0000_1000);
        assert_eq!(transport.bar_read(0x28, 4), 0x2000);
        assert_eq!(transport.bar_read(0x2C, 4), 0x3);
        // Vectors that the table does not have read as VIRTIO_MSI_NO_VECTOR.
        transport.bar_write(0x1A, 2, 2, |_, _| {});
        assert_eq!(transport.bar_read(0x1A, 2), 2);
        transport.bar_write(0x1A, 2, 3, |_, _| {});
        assert_eq!(transport.bar_read(0x1A, 2), 0xFFFF);
        // The device status is a byte, next to the configuration generation.
        transport.bar_write(0x14, 1, 1, |_, _| {});
        assert_eq!(transport.bar_read(0x14, 1), 1);
        assert_eq!(transport.bar_read(0x14, 2), 1);
        // The device's configuration space.
        assert_eq!(transport.bar_read(0x2001, 2), 0x0302);
    }

    #[test]
    fn counts_accesses_and_notifications() {
        let transport = Transport::new(Box::new(Dummy));
        transport.bar_read(0x12, 2);
        transport.bar_write(NOTIFY, 2, 0, |_, _| {});
        transport.bar_write(NOTIFY, 2, 0, |_, _| {});
        // There is no such queue.
        transport.bar_write(NOTIFY + 7 * NOTIFY_MULTIPLIER, 2, 7, |_, _| {});
        let mut counters = BTreeMap::new();
        transport.snapshot("dev", &mut counters);
        assert_eq!(counters["dev.mmio_reads"], 1);
        assert_eq!(counters["dev.mmio_writes"], 3);
        assert_eq!(counters["dev.queue0.notifies"], 2);
        assert_eq!(counters["dev.queue1.notifies"], 0);
        assert_eq!(counters["dev.irqs"], 0);
        assert_eq!(counters.len(), 8);
    }

    #[test]
    fn msix_table() {
        let mut msix = Msix::new(3);
        let sent = RefCell::new(Vec::new());
        let send = |address, data| sent.borrow_mut().push((address, data));
        // Masked (and MSI-X is disabled): the vector is pending.
        msix.write_table(0x10, 8, 0xFEE0_1000, &send);
        msix.write_table(0x18, 4, 0x41, &send);
        msix.signal(1, &send);
        assert_eq!(msix.read_pba(0, 8), 0b10);
        msix.set_control(MSIX_CONTROL_ENABLE, &send);
        assert!(sent.borrow().is_empty());
        // Unmasking the vector sends it.
        msix.write_table(0x1C, 4, 0, &send);
        assert_eq!(sent.take(), [(0xFEE0_1000, 0x41)]);
        assert_eq!(msix.read_pba(0, 8), 0);
        msix.signal(1, &send);
        assert_eq!(sent.take(), [(0xFEE0_1000, 0x41)]);
        // The function mask masks all vectors.
        msix.set_control(MSIX_CONTROL_ENABLE | MSIX_CONTROL_FUNCTION_MASK, &send);
        msix.signal(1, &send);
        msix.signal(7, &send);
        assert!(sent.borrow().is_empty());
        msix.set_control(MSIX_CONTROL_ENABLE, &send);
        assert_eq!(sent.take(), [(0xFEE0_1000, 0x41)]);
        // Reads: words, and a byte of the vector control.
        assert_eq!(msix.read_table(0x10, 4), 0xFEE0_1000);
        assert_eq!(msix.read_table(0x18, 8), 0x41);
        assert_eq!(msix.read_table(0x0C, 1), 1);
        // Of the vector control, only the mask bit can be set.
        msix.write_table(0x2C, 4, 0xFFFF_FFFF, &send);
        assert_eq!(msix.read_table(0x2C, 4), 1);
        // Narrow and misaligned writes are ignored.
        msix.write_table(0x18, 2, 0x42, &send);
        msix.write_table(0x14, 8, 0x42, &send);
        assert_eq!(msix.read_table(0x10, 8), 0xFEE0_1000);
        assert_eq!(msix.read_table(0x18, 4), 0x41);
    }

    #[test]
    fn queues_interrupt_with_their_vector() {
        let memory = memory();
        let transport = Transport::new(Box::new(Dummy));
        transport.set_msix_control(MSIX_CONTROL_ENABLE, |_, _| {});
        for vector in 0..2u64 {
            let entry = MSIX_TABLE + vector * MSIX_ENTRY_SIZE;
            transport.bar_write(entry, 4, 0xFEE0_0000, |_, _| {});
            transport.bar_write(entry + 8, 4, 0x40 + vector, |_, _| {});
            transport.bar_write(entry + 12, 4, 0, |_, _| {});
        }
        for queue in 0..2u64 {
            transport.bar_write(0x16, 2, queue, |_, _| {});
            transport.bar_write(0x1A, 2, queue, |_, _| {});
            transport.bar_write(0x28, 4, 0x2000 * (queue + 1), |_, _| {});
            transport.bar_write(0x30, 4, 0x3000 * (queue + 1), |_, _| {});
            transport.bar_write(0x1C, 2, 1, |_, _| {});
        }
        // Notify the second queue (at its own address).
        transport.bar_write(NOTIFY + NOTIFY_MULTIPLIER, 2, 1, |_, _| {});
        let sent = RefCell::new(Vec::new());
        futures_lite::future::block_on(futures_lite::future::or(
            transport.process_notified(&memory, |address, data| {
                sent.borrow_mut().push((address, data))
            }),
            futures_lite::future::ready(()),
        ));
        assert_eq!(sent.take(), [(0xFEE0_0000, 0x41)]);
        let mut counters = BTreeMap::new();
        transport.snapshot("dev", &mut counters);
        assert_eq!(counters["dev.irqs"], 1);
        assert_eq!(counters["dev.queue1.notifies"], 1);
        assert_eq!(counters["dev.queue1.used"], 1);
        // The ISR status is set, and a read clears it.
        assert_eq!(transport.bar_read(ISR, 1), 1);
        assert_eq!(transport.bar_read(ISR, 1), 0);
        // After a reset, the queues have no vectors.
        transport.bar_write(0x14, 1, 0, |_, _| {});
        transport.bar_write(0x16, 2, 1, |_, _| {});
        assert_eq!(transport.bar_read(0x1A, 2), 0xFFFF);
    }
}
