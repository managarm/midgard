//! A conventional PCI bus: bus 0 of segment 0, whose configuration space the guest
//! accesses through ECAM (the MCFG table points to it). Only function 0 of each slot
//! exists; slot 0 is the host bridge.
//!
//! There is no configuration mechanism #1 (ports 0xCF8 and 0xCFC): Linux would prefer it
//! for the first 256 bytes if it found it, and each access through it takes two exits
//! instead of one.

use std::sync::Mutex;

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

// Fields of the (type 0) header (their offsets in bytes).
const VENDOR_ID: usize = 0x00;
const DEVICE_ID: usize = 0x02;
const COMMAND: usize = 0x04;
const REVISION_ID: usize = 0x08;
const CLASS_CODE: usize = 0x09;

// Command register bits.
const COMMAND_MEMORY: u16 = 1 << 1;
const COMMAND_BUS_MASTER: u16 = 1 << 2;
const COMMAND_INTX_DISABLE: u16 = 1 << 10;

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
/// driver can write.
pub(crate) struct ConfigSpace {
    registers: [u32; NUM_REGISTERS],
    writable: [u32; NUM_REGISTERS],
}

impl ConfigSpace {
    /// A header of type 0; `class` is the base class, subclass and programming interface
    /// (from the most significant byte down).
    pub(crate) fn new(vendor: u16, device: u16, class: u32, revision: u8) -> Self {
        let mut space = Self {
            registers: [0; NUM_REGISTERS],
            writable: [0; NUM_REGISTERS],
        };
        space.set_u16(VENDOR_ID, vendor);
        space.set_u16(DEVICE_ID, device);
        space.set_bytes(REVISION_ID, &[revision]);
        space.set_bytes(CLASS_CODE, &class.to_le_bytes()[..3]);
        let command = COMMAND_MEMORY | COMMAND_BUS_MASTER | COMMAND_INTX_DISABLE;
        space.set_writable(COMMAND, command.into());
        space
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

    fn read(&self, register: usize) -> u32 {
        self.registers.get(register).copied().unwrap_or(0)
    }

    /// Writes the bits `enables` of a register.
    fn write(&mut self, register: usize, value: u32, enables: u32) {
        if register < NUM_REGISTERS {
            let mask = self.writable[register] & enables;
            self.registers[register] = (self.registers[register] & !mask) | (value & mask);
        }
    }
}

/// The bus, with the configuration space of each slot's function 0.
pub(crate) struct Bus {
    slots: Vec<Option<Mutex<ConfigSpace>>>,
}

impl Bus {
    /// A bus with the host bridge (in slot 0).
    pub(crate) fn new() -> Self {
        let mut slots: Vec<_> = (0..NUM_SLOTS).map(|_| None).collect();
        let host_bridge =
            ConfigSpace::new(HOST_BRIDGE_VENDOR, HOST_BRIDGE_DEVICE, CLASS_HOST_BRIDGE, 0);
        slots[0] = Some(Mutex::new(host_bridge));
        Self { slots }
    }

    /// The function that `offset` (into the ECAM region) addresses, and the offset into
    /// its configuration space.
    fn function(&self, offset: u64) -> Option<(&Mutex<ConfigSpace>, usize)> {
        let index = offset / ECAM_FUNCTION_SIZE;
        let (slot, function) = (index / 8, index % 8);
        if index >= FUNCTIONS_PER_BUS || function != 0 {
            return None;
        }
        let offset = (offset % ECAM_FUNCTION_SIZE) as usize;
        Some((self.slots[slot as usize].as_ref()?, offset))
    }

    /// Reads `size` bytes at `offset` of the ECAM region. Absent functions read as all
    /// ones, and so does what is not an access to one register (see [`Access`]).
    pub(crate) fn config_read(&self, offset: u64, size: u8) -> u64 {
        let Some((function, offset)) = self.function(offset) else {
            return u64::MAX;
        };
        let Some(access) = Access::new(offset, size) else {
            return u64::MAX;
        };
        let space = function.lock().unwrap();
        ((space.read(access.register) & access.enables) >> access.shift).into()
    }

    /// Writes `size` bytes at `offset` of the ECAM region; what is not an access to one
    /// register (see [`Access`]) writes nothing.
    pub(crate) fn config_write(&self, offset: u64, size: u8, value: u64) {
        let Some((function, offset)) = self.function(offset) else {
            return;
        };
        let Some(access) = Access::new(offset, size) else {
            return;
        };
        let mut space = function.lock().unwrap();
        let value = (value as u32) << access.shift;
        space.write(access.register, value, access.enables);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        bus.config_write(ecam(5, 0, COMMAND as u64), 2, 0xFFFF);
    }

    #[test]
    fn write_masks() {
        let bus = Bus::new();
        // The vendor ID is read-only; of the command register, only some bits can be set.
        bus.config_write(ecam(0, 0, 0), 4, 0xFFFF_FFFF);
        assert_eq!(bus.config_read(ecam(0, 0, 0), 4), 0x0008_1B36);
        bus.config_write(ecam(0, 0, COMMAND as u64), 2, 0xFFFF);
        assert_eq!(bus.config_read(ecam(0, 0, COMMAND as u64), 2), 0x0406);
        // Byte accesses.
        bus.config_write(ecam(0, 0, COMMAND as u64 + 1), 1, 0);
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
        bus.config_write(ecam(0, 0, 0), 8, u64::MAX);
        bus.config_write(ecam(0, 0, 3), 2, 0xFFFF);
        bus.config_write(ecam(0, 0, COMMAND as u64), 3, 0xFF_FFFF);
        assert_eq!(bus.config_read(ecam(0, 0, COMMAND as u64), 2), 0);
        // A word in the middle of a register is.
        assert_eq!(bus.config_read(ecam(0, 0, 1), 2), 0x081B);
    }
}
