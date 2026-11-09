//! An I/O APIC (24 pins) for backends whose hypervisor only emulates the local APIC
//! (Hel). Its interrupts go to the local APIC of the only vCPU: the backend raises the
//! [`Interrupt`]s that this model returns, and reports the EOIs of level-triggered
//! interrupts back to it.

/// The number of pins.
pub(crate) const NUM_PINS: usize = 24;

/// The registers in the MMIO window (offsets from the base address).
const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;

/// Like KVM's I/O APIC: version 0x11 (no EOI register), and the highest pin.
const VERSION: u32 = 0x11 | ((NUM_PINS as u32 - 1) << 16);

// Redirection table entry bits.
const RTE_VECTOR: u64 = 0xFF;
const RTE_DELIVERY_MODE: u64 = 7 << 8;
const RTE_DELIVERY_STATUS: u64 = 1 << 12;
const RTE_REMOTE_IRR: u64 = 1 << 14;
const RTE_LEVEL: u64 = 1 << 15;
const RTE_MASKED: u64 = 1 << 16;
const RTE_READ_ONLY: u64 = RTE_DELIVERY_STATUS | RTE_REMOTE_IRR;

/// An interrupt for the local APIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Interrupt {
    pub(crate) vector: u8,
    /// Level-triggered: the guest's EOI needs to reach [`Ioapic::eoi`].
    pub(crate) level: bool,
}

pub(crate) struct Ioapic {
    select: u32,
    id: u32,
    /// The levels of the pins (one bit per pin).
    lines: u32,
    redirection: [u64; NUM_PINS],
}

impl Ioapic {
    pub(crate) fn new() -> Self {
        Self {
            select: 0,
            id: 0,
            lines: 0,
            redirection: [RTE_MASKED; NUM_PINS],
        }
    }

    /// Sets the level of a pin. An edge-triggered pin interrupts on the rising edge, a
    /// level-triggered pin while its level is high (once per EOI).
    pub(crate) fn set_irq(&mut self, pin: usize, level: bool) -> Option<Interrupt> {
        let mask = 1 << pin;
        let was_high = self.lines & mask != 0;
        if level {
            self.lines |= mask;
        } else {
            self.lines &= !mask;
        }
        let triggered = if self.redirection[pin] & RTE_LEVEL != 0 {
            level
        } else {
            level && !was_high
        };
        if triggered { self.service(pin) } else { None }
    }

    /// The local APIC ended a level-triggered interrupt with this vector. Pins that are
    /// still high interrupt again.
    pub(crate) fn eoi(&mut self, vector: u8) -> Option<Interrupt> {
        let mut interrupt = None;
        for pin in 0..NUM_PINS {
            let entry = self.redirection[pin];
            if entry & RTE_LEVEL == 0 || entry & RTE_VECTOR != vector as u64 {
                continue;
            }
            self.redirection[pin] &= !RTE_REMOTE_IRR;
            if self.lines & (1 << pin) != 0 {
                interrupt = self.service(pin).or(interrupt);
            }
        }
        interrupt
    }

    /// Reads a 32-bit register of the MMIO window.
    pub(crate) fn read(&self, offset: u64) -> u32 {
        match offset {
            IOREGSEL => self.select,
            IOWIN => self.read_register(),
            _ => 0,
        }
    }

    /// Writes a 32-bit register of the MMIO window (unmasking a level-triggered pin that
    /// is high interrupts).
    pub(crate) fn write(&mut self, offset: u64, value: u32) -> Option<Interrupt> {
        match offset {
            IOREGSEL => {
                self.select = value & 0xFF;
                None
            }
            IOWIN => self.write_register(value),
            _ => None,
        }
    }

    /// The pin and half (high: bits 63:32) of the redirection entry that IOREGSEL selects.
    fn selected_entry(&self) -> Option<(usize, bool)> {
        let index = self.select.checked_sub(0x10)? as usize;
        let pin = index / 2;
        (pin < NUM_PINS).then_some((pin, index % 2 == 1))
    }

    fn read_register(&self) -> u32 {
        match self.select {
            0 => self.id,
            1 => VERSION,
            // The arbitration ID.
            2 => self.id,
            _ => match self.selected_entry() {
                Some((pin, true)) => (self.redirection[pin] >> 32) as u32,
                Some((pin, false)) => self.redirection[pin] as u32,
                None => 0,
            },
        }
    }

    fn write_register(&mut self, value: u32) -> Option<Interrupt> {
        if self.select == 0 {
            self.id = value & 0x0F00_0000;
            return None;
        }
        let (pin, high) = self.selected_entry()?;
        let old = self.redirection[pin];
        let mut entry = if high {
            (old & 0xFFFF_FFFF) | (value as u64) << 32
        } else {
            (old & !0xFFFF_FFFF) | value as u64
        };
        entry = (entry & !RTE_READ_ONLY) | (old & RTE_READ_ONLY);
        // Like KVM: an edge-triggered pin has no interrupt in service.
        if entry & RTE_LEVEL == 0 {
            entry &= !RTE_REMOTE_IRR;
        }
        self.redirection[pin] = entry;
        if entry & RTE_LEVEL != 0 && self.lines & (1 << pin) != 0 {
            self.service(pin)
        } else {
            None
        }
    }

    /// Sends the pin's interrupt unless it is masked or (level-triggered) in service.
    fn service(&mut self, pin: usize) -> Option<Interrupt> {
        let entry = self.redirection[pin];
        let vector = (entry & RTE_VECTOR) as u8;
        let level = entry & RTE_LEVEL != 0;
        // Fixed and lowest priority interrupts; the others (SMI, NMI, INIT and ExtINT) and
        // vectors that the local APIC rejects are not supported.
        let fixed = (entry & RTE_DELIVERY_MODE) >> 8 <= 1;
        if entry & RTE_MASKED != 0 || !fixed || vector < 16 {
            return None;
        }
        if level {
            if entry & RTE_REMOTE_IRR != 0 {
                return None;
            }
            self.redirection[pin] |= RTE_REMOTE_IRR;
        }
        Some(Interrupt { vector, level })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(ioapic: &mut Ioapic, pin: usize, low: u32) -> Option<Interrupt> {
        ioapic.write(IOREGSEL, 0x10 + 2 * pin as u32);
        ioapic.write(IOWIN, low)
    }

    fn entry(ioapic: &mut Ioapic, pin: usize) -> u32 {
        ioapic.write(IOREGSEL, 0x10 + 2 * pin as u32);
        ioapic.read(IOWIN)
    }

    #[test]
    fn version_and_reset() {
        let mut ioapic = Ioapic::new();
        ioapic.write(IOREGSEL, 1);
        assert_eq!(ioapic.read(IOWIN), 0x0017_0011);
        assert_eq!(entry(&mut ioapic, 5), RTE_MASKED as u32);
        // Masked pins do not interrupt.
        assert_eq!(ioapic.set_irq(5, true), None);
    }

    #[test]
    fn edge() {
        let mut ioapic = Ioapic::new();
        assert_eq!(program(&mut ioapic, 4, 0x30), None);
        let interrupt = Interrupt {
            vector: 0x30,
            level: false,
        };
        assert_eq!(ioapic.set_irq(4, true), Some(interrupt));
        // Only rising edges interrupt.
        assert_eq!(ioapic.set_irq(4, true), None);
        assert_eq!(ioapic.set_irq(4, false), None);
        assert_eq!(ioapic.set_irq(4, true), Some(interrupt));
    }

    #[test]
    fn level() {
        let mut ioapic = Ioapic::new();
        let low = 0x31 | RTE_LEVEL as u32;
        program(&mut ioapic, 9, low);
        let interrupt = Interrupt {
            vector: 0x31,
            level: true,
        };
        assert_eq!(ioapic.set_irq(9, true), Some(interrupt));
        // In service until the EOI.
        assert_eq!(ioapic.set_irq(9, true), None);
        assert_ne!(entry(&mut ioapic, 9) & RTE_REMOTE_IRR as u32, 0);
        // The pin is still high: the EOI interrupts again.
        assert_eq!(ioapic.eoi(0x31), Some(interrupt));
        ioapic.set_irq(9, false);
        assert_eq!(ioapic.eoi(0x31), None);
        assert_eq!(entry(&mut ioapic, 9) & RTE_REMOTE_IRR as u32, 0);
        // Writes cannot set the remote IRR bit.
        program(&mut ioapic, 9, low | RTE_REMOTE_IRR as u32);
        assert_eq!(entry(&mut ioapic, 9) & RTE_REMOTE_IRR as u32, 0);
    }

    #[test]
    fn unmask_level() {
        let mut ioapic = Ioapic::new();
        let low = 0x32 | RTE_LEVEL as u32;
        program(&mut ioapic, 13, low | RTE_MASKED as u32);
        assert_eq!(ioapic.set_irq(13, true), None);
        assert_eq!(
            program(&mut ioapic, 13, low),
            Some(Interrupt {
                vector: 0x32,
                level: true
            })
        );
    }

    #[test]
    fn unsupported_delivery() {
        let mut ioapic = Ioapic::new();
        // NMI.
        program(&mut ioapic, 3, 0x30 | 4 << 8);
        assert_eq!(ioapic.set_irq(3, true), None);
        // Illegal vector.
        program(&mut ioapic, 6, 0x05);
        assert_eq!(ioapic.set_irq(6, true), None);
    }
}
