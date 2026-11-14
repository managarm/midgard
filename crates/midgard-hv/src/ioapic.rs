//! An I/O APIC (24 pins) for backends whose hypervisor only emulates the local APICs
//! (Hel, and KVM with a split irqchip): the backend sends the [`Interrupt`]s that this
//! model returns to the local APICs, and reports the EOIs of level-triggered interrupts
//! back to it.

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
const RTE_LOWEST_PRIORITY: u64 = 1 << 8;
const RTE_LOGICAL: u64 = 1 << 11;
const RTE_DELIVERY_STATUS: u64 = 1 << 12;
const RTE_REMOTE_IRR: u64 = 1 << 14;
const RTE_LEVEL: u64 = 1 << 15;
const RTE_MASKED: u64 = 1 << 16;
const RTE_DESTINATION_SHIFT: u32 = 56;
const RTE_READ_ONLY: u64 = RTE_DELIVERY_STATUS | RTE_REMOTE_IRR;

/// An interrupt for the local APIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Interrupt {
    pub(crate) vector: u8,
    /// Level-triggered: the guest's EOI needs to reach [`Ioapic::eoi`].
    pub(crate) level: bool,
    /// The local APIC ID (or, in logical mode, the logical destination).
    pub(crate) destination: u8,
    pub(crate) logical: bool,
    /// Lowest priority instead of fixed delivery.
    pub(crate) lowest_priority: bool,
}

impl Interrupt {
    /// Decodes an MSI (see [`crate::x86::msi`]). Fixed and lowest priority interrupts are
    /// supported; the others (SMI, NMI, INIT and ExtINT), vectors that the local APIC
    /// rejects and addresses outside of the MSI window are not.
    #[cfg(any(target_os = "managarm", test))]
    pub(crate) fn from_msi(address: u64, data: u32) -> Option<Self> {
        use crate::x86::msi;
        let vector = (data & msi::DATA_VECTOR) as u8;
        let fixed = (data & msi::DATA_DELIVERY_MODE) >> 8 <= 1;
        if address & msi::ADDRESS_MASK != msi::ADDRESS_BASE || !fixed || vector < 16 {
            return None;
        }
        Some(Self {
            vector,
            level: data & msi::DATA_LEVEL != 0,
            destination: (address >> msi::ADDRESS_DESTINATION_SHIFT) as u8,
            logical: address & msi::ADDRESS_LOGICAL != 0,
            lowest_priority: data & msi::DATA_DELIVERY_MODE == msi::DATA_LOWEST_PRIORITY,
        })
    }

    /// Whether the interrupt goes to the local APIC with x2APIC ID `id`, which is in
    /// x2APIC mode (on Hel, it always is). Without interrupt remapping, destinations have
    /// 8 bits, and the broadcast destination addresses all local APICs (like on KVM). In
    /// logical mode, the destination is a bitmask of cluster 0: an x2APIC's logical ID is
    /// its cluster (bits 31:4 of its ID) and one bit for its position in the cluster
    /// (bits 3:0). Lowest priority delivery picks one of the local APICs that match.
    #[cfg(any(target_os = "managarm", test))]
    pub(crate) fn targets(&self, id: u32) -> bool {
        let destination = u32::from(self.destination);
        if self.destination == crate::x86::msi::BROADCAST {
            true
        } else if self.logical {
            id >> 4 == 0 && destination & 1 << (id & 0xF) != 0
        } else {
            destination == id
        }
    }
}

pub(crate) struct Ioapic {
    select: u32,
    id: u32,
    /// The levels of the pins (one bit per pin).
    lines: u32,
    redirection: [u64; NUM_PINS],
    /// Whether a local APIC accepts an interrupt (see [`Ioapic::new`]).
    accepts: fn(&Interrupt) -> bool,
}

impl Ioapic {
    /// `accepts` says whether a local APIC accepts an interrupt, i.e., whether its
    /// destination addresses one. Interrupts that none accepts go nowhere and, if
    /// level-triggered, leave Remote IRR clear (like KVM's I/O APIC does), such that the
    /// pin interrupts once the guest points it at a local APIC that exists.
    pub(crate) fn new(accepts: fn(&Interrupt) -> bool) -> Self {
        Self {
            select: 0,
            id: 0,
            lines: 0,
            redirection: [RTE_MASKED; NUM_PINS],
            accepts,
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

    /// The interrupt that the pin sends (when its level says so), unless it is masked.
    pub(crate) fn redirection(&self, pin: usize) -> Option<Interrupt> {
        let entry = self.redirection[pin];
        let vector = (entry & RTE_VECTOR) as u8;
        // Fixed and lowest priority interrupts; the others (SMI, NMI, INIT and ExtINT) and
        // vectors that the local APIC rejects are not supported.
        let fixed = (entry & RTE_DELIVERY_MODE) >> 8 <= 1;
        if entry & RTE_MASKED != 0 || !fixed || vector < 16 {
            return None;
        }
        Some(Interrupt {
            vector,
            level: entry & RTE_LEVEL != 0,
            destination: (entry >> RTE_DESTINATION_SHIFT) as u8,
            logical: entry & RTE_LOGICAL != 0,
            lowest_priority: entry & RTE_DELIVERY_MODE == RTE_LOWEST_PRIORITY,
        })
    }

    /// Sends the pin's interrupt unless it is masked, (level-triggered) in service or no
    /// local APIC accepts it.
    fn service(&mut self, pin: usize) -> Option<Interrupt> {
        let interrupt = self.redirection(pin).filter(self.accepts)?;
        if interrupt.level {
            if self.redirection[pin] & RTE_REMOTE_IRR != 0 {
                return None;
            }
            self.redirection[pin] |= RTE_REMOTE_IRR;
        }
        Some(interrupt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(ioapic: &mut Ioapic, pin: usize, low: u32) -> Option<Interrupt> {
        ioapic.write(IOREGSEL, 0x10 + 2 * pin as u32);
        ioapic.write(IOWIN, low)
    }

    fn interrupt(vector: u8, level: bool) -> Interrupt {
        Interrupt {
            vector,
            level,
            destination: 0,
            logical: false,
            lowest_priority: false,
        }
    }

    fn entry(ioapic: &mut Ioapic, pin: usize) -> u32 {
        ioapic.write(IOREGSEL, 0x10 + 2 * pin as u32);
        ioapic.read(IOWIN)
    }

    #[test]
    fn version_and_reset() {
        let mut ioapic = Ioapic::new(|_| true);
        ioapic.write(IOREGSEL, 1);
        assert_eq!(ioapic.read(IOWIN), 0x0017_0011);
        assert_eq!(entry(&mut ioapic, 5), RTE_MASKED as u32);
        // Masked pins do not interrupt.
        assert_eq!(ioapic.set_irq(5, true), None);
    }

    #[test]
    fn edge() {
        let mut ioapic = Ioapic::new(|_| true);
        assert_eq!(program(&mut ioapic, 4, 0x30), None);
        let interrupt = interrupt(0x30, false);
        assert_eq!(ioapic.set_irq(4, true), Some(interrupt));
        // Only rising edges interrupt.
        assert_eq!(ioapic.set_irq(4, true), None);
        assert_eq!(ioapic.set_irq(4, false), None);
        assert_eq!(ioapic.set_irq(4, true), Some(interrupt));
    }

    #[test]
    fn level() {
        let mut ioapic = Ioapic::new(|_| true);
        let low = 0x31 | RTE_LEVEL as u32;
        program(&mut ioapic, 9, low);
        let interrupt = interrupt(0x31, true);
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
        let mut ioapic = Ioapic::new(|_| true);
        let low = 0x32 | RTE_LEVEL as u32;
        program(&mut ioapic, 13, low | RTE_MASKED as u32);
        assert_eq!(ioapic.set_irq(13, true), None);
        assert_eq!(program(&mut ioapic, 13, low), Some(interrupt(0x32, true)));
    }

    #[test]
    fn destination() {
        let mut ioapic = Ioapic::new(|_| true);
        // Lowest priority, logical destination 0x0C (in the high half).
        program(
            &mut ioapic,
            7,
            0x33 | (RTE_LOWEST_PRIORITY | RTE_LOGICAL) as u32,
        );
        ioapic.write(IOREGSEL, 0x10 + 2 * 7 + 1);
        ioapic.write(IOWIN, 0x0C00_0000);
        assert_eq!(
            ioapic.set_irq(7, true),
            Some(Interrupt {
                vector: 0x33,
                level: false,
                destination: 0x0C,
                logical: true,
                lowest_priority: true,
            })
        );
    }

    #[test]
    fn msi() {
        // Linux's MSI to APIC ID 3: fixed, edge-triggered.
        assert_eq!(
            Interrupt::from_msi(0xFEE0_3000, 0x0041),
            Some(Interrupt {
                destination: 3,
                ..interrupt(0x41, false)
            })
        );
        assert_eq!(
            Interrupt::from_msi(0xFEE0_0C04, 0x0142),
            Some(Interrupt {
                vector: 0x42,
                level: false,
                destination: 0,
                logical: true,
                lowest_priority: true,
            })
        );
        // NMI, an illegal vector and addresses outside of the MSI window.
        assert_eq!(Interrupt::from_msi(0xFEE0_0000, 0x0402), None);
        assert_eq!(Interrupt::from_msi(0xFEE0_0000, 0x0005), None);
        assert_eq!(Interrupt::from_msi(0xFED0_0000, 0x0041), None);
        assert_eq!(Interrupt::from_msi(0x1_FEE0_0000, 0x0041), None);
    }

    #[test]
    fn destinations() {
        let target = |address| Interrupt::from_msi(address, 0x41).unwrap();
        // Physical: the x2APIC ID, or broadcast.
        assert!(target(0xFEE0_0000).targets(0));
        assert!(!target(0xFEE0_1000).targets(0));
        assert!(target(0xFEE0_1000).targets(1));
        assert!(target(0xFEE0_0000 | 0xFF << 12).targets(0));
        // Logical: a bitmask of cluster 0.
        assert!(target(0xFEE0_1004).targets(0));
        assert!(!target(0xFEE0_2004).targets(0));
        assert!(target(0xFEE0_2004).targets(1));
        assert!(target(0xFEE8_0004).targets(7));
        assert!(!target(0xFEE0_1004).targets(16));
        assert!(target(0xFEEF_F004).targets(16));
    }

    #[test]
    fn undeliverable() {
        // Only the local APIC with x2APIC ID 0 exists.
        let mut ioapic = Ioapic::new(|interrupt| interrupt.targets(0));
        let low = 0x34 | RTE_LEVEL as u32;
        program(&mut ioapic, 10, low);
        ioapic.write(IOREGSEL, 0x10 + 2 * 10 + 1);
        ioapic.write(IOWIN, 0x0100_0000);
        // To APIC ID 1: the interrupt goes nowhere and is not in service.
        assert_eq!(ioapic.set_irq(10, true), None);
        assert_eq!(entry(&mut ioapic, 10) & RTE_REMOTE_IRR as u32, 0);
        // Pointing the pin at APIC ID 0 interrupts (the pin is still high).
        ioapic.write(IOREGSEL, 0x10 + 2 * 10 + 1);
        assert_eq!(ioapic.write(IOWIN, 0), Some(interrupt(0x34, true)));
        assert_ne!(entry(&mut ioapic, 10) & RTE_REMOTE_IRR as u32, 0);
    }

    #[test]
    fn unsupported_delivery() {
        let mut ioapic = Ioapic::new(|_| true);
        // NMI.
        program(&mut ioapic, 3, 0x30 | 4 << 8);
        assert_eq!(ioapic.set_irq(3, true), None);
        // Illegal vector.
        program(&mut ioapic, 6, 0x05);
        assert_eq!(ioapic.set_irq(6, true), None);
    }
}
