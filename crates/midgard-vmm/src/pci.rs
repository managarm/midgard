//! A conventional PCI bus: bus 0 of segment 0, whose configuration space the guest
//! accesses through ECAM (the MCFG table points to it). Only function 0 of each slot
//! exists; slot 0 is the host bridge.
//!
//! There is no configuration mechanism #1 (ports 0xCF8 and 0xCFC): Linux would prefer it
//! for the first 256 bytes if it found it, and each access through it takes two exits
//! instead of one.
//!
//! Like firmware would, midgard assigns the BARs (32-bit memory BARs, in the host bridge's
//! window) and enables their decoding before the guest starts. The guest may move them;
//! the bus decodes the addresses that the BARs hold (see [`Bus::find_bar`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// The configuration space of one function in the ECAM region.
const ECAM_FUNCTION_SIZE: u64 = 0x1000;
/// The functions of a bus: 32 slots with 8 functions each.
const FUNCTIONS_PER_BUS: u64 = 32 * 8;
/// The ECAM region of the bus.
pub(crate) const ECAM_SIZE: u64 = FUNCTIONS_PER_BUS * ECAM_FUNCTION_SIZE;
pub(crate) const NUM_SLOTS: usize = 32;
/// The configuration space of a conventional function; the rest of its ECAM page reads
/// as zero (Linux does not look there for functions that are not PCIe).
const CONFIG_SIZE: usize = 256;
/// Its registers, which are dwords.
const NUM_REGISTERS: usize = CONFIG_SIZE / 4;
pub(crate) const NUM_BARS: usize = 6;

// Fields of the (type 0) header (their offsets in bytes).
const VENDOR_ID: usize = 0x00;
const DEVICE_ID: usize = 0x02;
const COMMAND: usize = 0x04;
const STATUS: usize = 0x06;
const REVISION_ID: usize = 0x08;
const CLASS_CODE: usize = 0x09;
const BAR0: usize = 0x10;
const SUBSYSTEM_VENDOR_ID: usize = 0x2C;
const SUBSYSTEM_ID: usize = 0x2E;
const CAPABILITIES_POINTER: usize = 0x34;
const INTERRUPT_LINE: usize = 0x3C;
/// Where the capabilities start (after the header).
const FIRST_CAPABILITY: usize = 0x40;

// Command register bits.
const COMMAND_MEMORY: u16 = 1 << 1;
const COMMAND_BUS_MASTER: u16 = 1 << 2;
const COMMAND_INTX_DISABLE: u16 = 1 << 10;
/// Status register: there is a capabilities list.
const STATUS_CAPABILITIES: u16 = 1 << 4;

/// The MSI-X capability.
pub(crate) const CAP_MSIX: u8 = 0x11;
/// The MSI-X capability's Message Control register (after the ID and next pointer).
const MSIX_CONTROL: usize = 2;
/// Of Message Control, the driver writes the enable and function mask bits.
const MSIX_CONTROL_WRITABLE: u16 = 0xC000;

/// Red Hat's vendor ID for virtual devices and the device ID of QEMU's host bridge.
const HOST_BRIDGE_VENDOR: u16 = 0x1B36;
const HOST_BRIDGE_DEVICE: u16 = 0x0008;
/// Base class 6 (bridge), subclass 0 (host bridge).
const CLASS_HOST_BRIDGE: u32 = 0x06_0000;

/// A configuration access as a function sees it on the bus: to one register, with byte
/// enables. Accesses of 1, 2 or 4 bytes are of this form unless they span two registers.
struct Access {
    register: usize,
    /// The bits of the register that the byte enables select.
    enables: u32,
    /// Where in the register the access' value is.
    shift: u32,
}

impl Access {
    /// The access to `size` bytes at `offset` of a function's configuration space.
    fn new(offset: usize, size: u8) -> Option<Self> {
        let byte = offset % 4;
        if !matches!(size, 1 | 2 | 4) || byte + usize::from(size) > 4 {
            return None;
        }
        let shift = 8 * byte as u32;
        Some(Self {
            register: offset / 4,
            enables: (u32::MAX >> (32 - 8 * u32::from(size))) << shift,
            shift,
        })
    }
}

/// The configuration space of a function: its registers and which of their bits the
/// driver can write. It lays out the header and the capabilities; what follows from the
/// driver's writes is up to the [`Function`].
pub(crate) struct ConfigSpace {
    registers: [u32; NUM_REGISTERS],
    writable: [u32; NUM_REGISTERS],
    /// The last capability (whose next pointer the next one goes into).
    last_capability: Option<usize>,
    /// Where the next capability goes.
    next_capability: usize,
    /// The MSI-X capability.
    msix: Option<usize>,
}

impl ConfigSpace {
    /// A header of type 0; `class` is the base class, subclass and programming interface
    /// (from the most significant byte down). The function has no INTx pin.
    pub(crate) fn new(vendor: u16, device: u16, class: u32, revision: u8) -> Self {
        let mut space = Self {
            registers: [0; NUM_REGISTERS],
            writable: [0; NUM_REGISTERS],
            last_capability: None,
            next_capability: FIRST_CAPABILITY,
            msix: None,
        };
        space.set_u16(VENDOR_ID, vendor);
        space.set_u16(DEVICE_ID, device);
        space.set_bytes(REVISION_ID, &[revision]);
        space.set_bytes(CLASS_CODE, &class.to_le_bytes()[..3]);
        let command = COMMAND_MEMORY | COMMAND_BUS_MASTER | COMMAND_INTX_DISABLE;
        space.set_writable(COMMAND, command.into());
        // Scratch space for software.
        space.set_writable(INTERRUPT_LINE, 0xFF);
        space
    }

    /// The 16-bit field at `offset` (in bytes), which is within one register.
    fn u16(&self, offset: usize) -> u16 {
        debug_assert!(offset % 4 <= 2, "the field spans registers");
        (self.registers[offset / 4] >> (8 * (offset % 4))) as u16
    }

    /// Sets the bytes from `offset` on.
    fn set_bytes(&mut self, offset: usize, bytes: &[u8]) {
        for (offset, byte) in (offset..).zip(bytes) {
            let shift = 8 * (offset % 4);
            let register = &mut self.registers[offset / 4];
            *register = (*register & !(0xFF << shift)) | u32::from(*byte) << shift;
        }
    }

    fn set_u16(&mut self, offset: usize, value: u16) {
        self.set_bytes(offset, &value.to_le_bytes());
    }

    /// Lets the driver write the bits `mask` of the field at `offset` (in bytes), which
    /// is within one register.
    fn set_writable(&mut self, offset: usize, mask: u32) {
        let shift = 8 * (offset % 4);
        debug_assert_eq!(mask << shift >> shift, mask, "the field spans registers");
        self.writable[offset / 4] |= mask << shift;
    }

    pub(crate) fn set_subsystem(&mut self, vendor: u16, device: u16) {
        self.set_u16(SUBSYSTEM_VENDOR_ID, vendor);
        self.set_u16(SUBSYSTEM_ID, device);
    }

    /// A 32-bit, non-prefetchable memory BAR of `size` bytes (a power of two) at
    /// `address`, which the function decodes from the start.
    pub(crate) fn add_memory_bar(&mut self, index: usize, address: u32, size: u32) {
        assert!(size.is_power_of_two() && size >= 0x10 && address.is_multiple_of(size));
        self.registers[BAR0 / 4 + index] = address;
        // The address bits above the BAR's size; the low bits say what kind of BAR it
        // is (a 32-bit memory BAR: all zero).
        self.writable[BAR0 / 4 + index] = !(size - 1);
        let command = self.u16(COMMAND) | COMMAND_MEMORY;
        self.set_u16(COMMAND, command);
    }

    /// The size of a BAR; 0 if it does not exist. Like the driver sees it: from the
    /// address bits that can be written.
    fn bar_size(&self, index: usize) -> u32 {
        (!self.writable[BAR0 / 4 + index]).wrapping_add(1)
    }

    /// Appends a capability: its ID, then `body` (after the ID and the next pointer).
    /// Returns its offset.
    pub(crate) fn add_capability(&mut self, id: u8, body: &[u8]) -> usize {
        let offset = self.next_capability;
        // Capabilities start at a register (which [`Function::write`] relies on).
        debug_assert!(offset.is_multiple_of(4));
        assert!(
            offset + 2 + body.len() <= CONFIG_SIZE,
            "too many capabilities"
        );
        self.set_bytes(offset, &[id]);
        self.set_bytes(offset + 2, body);
        match self.last_capability {
            Some(last) => self.set_bytes(last + 1, &[offset as u8]),
            None => {
                self.set_bytes(CAPABILITIES_POINTER, &[offset as u8]);
                let status = self.u16(STATUS) | STATUS_CAPABILITIES;
                self.set_u16(STATUS, status);
            }
        }
        if id == CAP_MSIX {
            self.set_writable(offset + MSIX_CONTROL, MSIX_CONTROL_WRITABLE.into());
            self.msix = Some(offset);
        }
        self.last_capability = Some(offset);
        self.next_capability = (offset + 2 + body.len()).next_multiple_of(4);
        offset
    }

    fn read(&self, register: usize) -> u32 {
        self.registers.get(register).copied().unwrap_or(0)
    }

    /// Writes the bits `enables` of a register. Returns its old and its new value.
    fn write(&mut self, register: usize, value: u32, enables: u32) -> (u32, u32) {
        if register >= NUM_REGISTERS {
            return (0, 0);
        }
        let mask = self.writable[register] & enables;
        let old = self.registers[register];
        let new = (old & !mask) | (value & mask);
        self.registers[register] = new;
        (old, new)
    }
}

/// A BAR of a function.
struct Bar {
    /// The BAR's size (which never changes); 0 if the BAR does not exist.
    size: u32,
    /// The address that the BAR decodes (zero: none), which [`Bus::find_bar`] reads
    /// without the configuration space's lock.
    address: AtomicU64,
}

/// A function on the bus: its configuration space and what follows from its registers.
struct Function {
    config: Mutex<ConfigSpace>,
    bars: [Bar; NUM_BARS],
}

impl Function {
    fn new(config: ConfigSpace) -> Self {
        let bars = std::array::from_fn(|index| Bar {
            size: config.bar_size(index),
            address: AtomicU64::new(0),
        });
        let function = Self {
            config: Mutex::new(config),
            bars,
        };
        function.update_bars(&function.config.lock().unwrap());
        function
    }

    /// Sets the addresses that the BARs decode from the registers.
    fn update_bars(&self, space: &ConfigSpace) {
        let enabled = space.u16(COMMAND) & COMMAND_MEMORY != 0;
        for (index, bar) in self.bars.iter().enumerate() {
            // Like on hardware, this is also so while the driver sizes the BAR (which is
            // why drivers disable decoding for that).
            let address = space.read(BAR0 / 4 + index);
            let address = if enabled { u64::from(address) } else { 0 };
            bar.address.store(address, Ordering::SeqCst);
        }
    }

    /// Writes the bits `enables` of a register of `space` (the function's configuration
    /// space). Returns the Message Control register of the MSI-X capability if the write
    /// changed it.
    fn write(
        &self,
        space: &mut ConfigSpace,
        register: usize,
        value: u32,
        enables: u32,
    ) -> Option<u16> {
        let (old, new) = space.write(register, value, enables);
        if new == old {
            return None;
        }
        let offset = 4 * register;
        if offset == COMMAND || (BAR0..BAR0 + 4 * NUM_BARS).contains(&offset) {
            self.update_bars(space);
        }
        // Of the capability's first register, only Message Control can be written.
        (space.msix == Some(offset)).then_some((new >> (8 * MSIX_CONTROL)) as u16)
    }
}

/// What a write to a function's configuration space did beyond changing its registers,
/// which the function's device needs to know.
///
/// It holds the configuration space's lock: whoever applies the effects to the device
/// does so before dropping them, such that the device sees the effects of concurrent
/// writes in the order of the writes.
pub(crate) struct WriteEffects<'a> {
    /// The function's slot.
    pub(crate) slot: usize,
    /// The Message Control register of the MSI-X capability, if the write changed it.
    pub(crate) msix_control: Option<u16>,
    _config: MutexGuard<'a, ConfigSpace>,
}

/// The bus, with the configuration space of each slot's function 0.
pub(crate) struct Bus {
    slots: Vec<Option<Function>>,
}

impl Bus {
    /// A bus with the host bridge (in slot 0).
    pub(crate) fn new() -> Self {
        let mut slots: Vec<_> = (0..NUM_SLOTS).map(|_| None).collect();
        let host_bridge =
            ConfigSpace::new(HOST_BRIDGE_VENDOR, HOST_BRIDGE_DEVICE, CLASS_HOST_BRIDGE, 0);
        slots[0] = Some(Function::new(host_bridge));
        Self { slots }
    }

    /// Puts a function into `slot` (which must be free).
    pub(crate) fn add(&mut self, slot: usize, config: ConfigSpace) {
        assert!(self.slots[slot].is_none(), "PCI slot {slot} is taken");
        self.slots[slot] = Some(Function::new(config));
    }

    /// The function that `offset` (into the ECAM region) addresses: its slot, and the
    /// offset into its configuration space.
    fn function(&self, offset: u64) -> Option<(usize, &Function, usize)> {
        let index = offset / ECAM_FUNCTION_SIZE;
        let (slot, function) = ((index / 8) as usize, index % 8);
        if index >= FUNCTIONS_PER_BUS || function != 0 {
            return None;
        }
        let offset = (offset % ECAM_FUNCTION_SIZE) as usize;
        Some((slot, self.slots[slot].as_ref()?, offset))
    }

    /// Reads `size` bytes at `offset` of the ECAM region. Absent functions read as all
    /// ones, and so does what is not an access to one register (see [`Access`]).
    pub(crate) fn config_read(&self, offset: u64, size: u8) -> u64 {
        let Some((_, function, offset)) = self.function(offset) else {
            return u64::MAX;
        };
        let Some(access) = Access::new(offset, size) else {
            return u64::MAX;
        };
        let space = function.config.lock().unwrap();
        ((space.read(access.register) & access.enables) >> access.shift).into()
    }

    /// Writes `size` bytes at `offset` of the ECAM region; what is not an access to one
    /// register (see [`Access`]) writes nothing. Returns the write's effects, which the
    /// caller applies to the function's device; `None` if it wrote to no function.
    #[must_use]
    pub(crate) fn config_write(
        &self,
        offset: u64,
        size: u8,
        value: u64,
    ) -> Option<WriteEffects<'_>> {
        let (slot, function, offset) = self.function(offset)?;
        let access = Access::new(offset, size)?;
        let mut config = function.config.lock().unwrap();
        let value = (value as u32) << access.shift;
        let msix_control = function.write(&mut config, access.register, value, access.enables);
        Some(WriteEffects {
            slot,
            msix_control,
            _config: config,
        })
    }

    /// The BAR that decodes `address`: its function's slot, the BAR and the offset into
    /// it.
    pub(crate) fn find_bar(&self, address: u64) -> Option<(usize, usize, u64)> {
        for (slot, function) in self.slots.iter().enumerate() {
            let Some(function) = function else {
                continue;
            };
            for (index, bar) in function.bars.iter().enumerate() {
                let base = bar.address.load(Ordering::Relaxed);
                if base != 0 && (base..base + u64::from(bar.size)).contains(&address) {
                    return Some((slot, index, address - base));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes the ECAM region; returns what the write's effects tell about MSI-X.
    fn write(bus: &Bus, offset: u64, size: u8, value: u64) -> Option<(usize, u16)> {
        let effects = bus.config_write(offset, size, value)?;
        Some((effects.slot, effects.msix_control?))
    }

    /// The offset of a register of a function (on bus 0) in the ECAM region.
    fn ecam(slot: u64, function: u64, register: u64) -> u64 {
        slot << 15 | function << 12 | register
    }

    #[test]
    fn host_bridge() {
        let bus = Bus::new();
        assert_eq!(bus.config_read(ecam(0, 0, 0), 4), 0x0008_1B36);
        assert_eq!(bus.config_read(ecam(0, 0, 0x08), 4), 0x0600_0000);
        // Past the header and the conventional configuration space.
        assert_eq!(bus.config_read(ecam(0, 0, 0x40), 4), 0);
        assert_eq!(bus.config_read(ecam(0, 0, 0x100), 4), 0);
    }

    #[test]
    fn absent_functions() {
        let bus = Bus::new();
        assert_eq!(bus.config_read(ecam(1, 0, 0), 4) as u32, u32::MAX);
        assert_eq!(bus.config_read(ecam(0, 1, 0), 2) as u16, u16::MAX);
        // Writes go nowhere.
        let command = ecam(5, 0, COMMAND as u64);
        assert!(bus.config_write(command, 2, 0xFFFF).is_none());
        assert!(
            bus.config_write(ecam(0, 1, COMMAND as u64), 2, 0xFFFF)
                .is_none()
        );
        assert!(
            bus.config_write(ecam(0, 0, COMMAND as u64), 2, 0xFFFF)
                .is_some()
        );
    }

    #[test]
    fn write_masks() {
        let bus = Bus::new();
        // The vendor ID is read-only; of the command register, only some bits can be set.
        write(&bus, ecam(0, 0, 0), 4, 0xFFFF_FFFF);
        assert_eq!(bus.config_read(ecam(0, 0, 0), 4), 0x0008_1B36);
        write(&bus, ecam(0, 0, COMMAND as u64), 2, 0xFFFF);
        assert_eq!(bus.config_read(ecam(0, 0, COMMAND as u64), 2), 0x0406);
        // Byte accesses.
        write(&bus, ecam(0, 0, COMMAND as u64 + 1), 1, 0);
        assert_eq!(bus.config_read(ecam(0, 0, COMMAND as u64), 1), 0x06);
        assert_eq!(bus.config_read(ecam(0, 0, COMMAND as u64), 2), 0x0006);
        assert_eq!(bus.config_read(ecam(0, 0, CLASS_CODE as u64 + 2), 1), 0x06);
    }

    #[test]
    fn accesses_that_span_registers() {
        let bus = Bus::new();
        // Neither eight bytes nor a word across two registers are an access to one
        // register: they read as all ones and write nothing.
        assert_eq!(bus.config_read(ecam(0, 0, 0), 8), u64::MAX);
        assert_eq!(bus.config_read(ecam(0, 0, 3), 2), u64::MAX);
        assert!(bus.config_write(ecam(0, 0, 0), 8, u64::MAX).is_none());
        assert!(bus.config_write(ecam(0, 0, 3), 2, 0xFFFF).is_none());
        let command = ecam(0, 0, COMMAND as u64);
        assert!(bus.config_write(command, 3, 0xFF_FFFF).is_none());
        assert_eq!(bus.config_read(ecam(0, 0, COMMAND as u64), 2), 0);
        // A word in the middle of a register is.
        assert_eq!(bus.config_read(ecam(0, 0, 1), 2), 0x081B);
    }

    fn device() -> ConfigSpace {
        let mut space = ConfigSpace::new(0x1AF4, 0x1042, 0x01_8000, 1);
        space.add_memory_bar(0, 0xE000_8000, 0x8000);
        space
    }

    #[test]
    fn bars() {
        let mut bus = Bus::new();
        bus.add(3, device());
        let bar0 = ecam(3, 0, BAR0 as u64);
        assert_eq!(bus.find_bar(0xE000_8010), Some((3, 0, 0x10)));
        assert_eq!(bus.find_bar(0xE001_0000), None);
        // Sizing: with decoding disabled, the driver writes all ones and reads the size.
        write(&bus, ecam(3, 0, COMMAND as u64), 2, 0);
        assert_eq!(bus.find_bar(0xE000_8010), None);
        write(&bus, bar0, 4, 0xFFFF_FFFF);
        assert_eq!(bus.config_read(bar0, 4), 0xFFFF_8000);
        // The driver moves the BAR and enables decoding.
        write(&bus, bar0, 4, 0xE004_0000);
        write(&bus, ecam(3, 0, COMMAND as u64), 2, COMMAND_MEMORY.into());
        assert_eq!(bus.find_bar(0xE000_8010), None);
        assert_eq!(bus.find_bar(0xE004_7FFF), Some((3, 0, 0x7FFF)));
        // A write of a part of the BAR moves it, too.
        write(&bus, bar0 + 3, 1, 0xD0);
        assert_eq!(bus.config_read(bar0, 4), 0xD004_0000);
        assert_eq!(bus.find_bar(0xE004_0000), None);
        assert_eq!(bus.find_bar(0xD004_0000), Some((3, 0, 0)));
        // Sized with decoding enabled, the BAR decodes where its address bits put it.
        write(&bus, bar0, 4, 0xFFFF_FFFF);
        assert_eq!(bus.find_bar(0xD004_0000), None);
        assert_eq!(bus.find_bar(0xFFFF_FFFF), Some((3, 0, 0x7FFF)));
        // BARs that do not exist stay zero.
        write(&bus, bar0 + 4, 4, 0xFFFF_FFFF);
        assert_eq!(bus.config_read(bar0 + 4, 4), 0);
    }

    #[test]
    fn capabilities() {
        let mut space = device();
        let vendor = space.add_capability(0x09, &[16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let msix = space.add_capability(CAP_MSIX, &[3, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!((vendor, msix), (0x40, 0x50));
        let mut bus = Bus::new();
        bus.add(1, space);
        assert_ne!(
            bus.config_read(ecam(1, 0, STATUS as u64), 2) as u16 & STATUS_CAPABILITIES,
            0
        );
        assert_eq!(
            bus.config_read(ecam(1, 0, CAPABILITIES_POINTER as u64), 1),
            0x40
        );
        assert_eq!(bus.config_read(ecam(1, 0, 0x40), 2), 0x5009);
        assert_eq!(bus.config_read(ecam(1, 0, 0x50), 2), 0x0011);
        // Enabling MSI-X: only the enable and function mask bits can be written, and the
        // device hears about it.
        let control = ecam(1, 0, 0x52);
        assert_eq!(write(&bus, control, 2, 0xFFFF), Some((1, 0xC003)));
        assert_eq!(bus.config_read(control, 2), 0xC003);
        assert_eq!(write(&bus, control, 2, 0xFFFF), None);
        assert_eq!(write(&bus, control - 2, 4, 0x8000_0000), Some((1, 0x8003)));
        // The low byte of Message Control is read-only.
        assert_eq!(write(&bus, control, 1, 0xFF), None);
        assert_eq!(write(&bus, control + 1, 1, 0x40), Some((1, 0x4003)));
    }
}
