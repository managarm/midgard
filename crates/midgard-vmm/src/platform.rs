//! The guest's machine: where midgard puts things in the guest-physical address space,
//! which I/O ports and which interrupt lines its devices use. Keeping them in one place
//! makes it easy to see that nothing overlaps.

// Guest-physical memory. RAM starts at zero; the MMIO hole is at the top of the 4 GiB.

/// The legacy video and BIOS areas (reserved in the E820 map), up to [`HIGH_MEMORY`].
pub const LEGACY_HOLE: u64 = 0xA_0000;
/// The ACPI tables (the RSDP first) are in the BIOS area, where Linux searches for the
/// RSDP; the zero page also points to them.
pub const ACPI_TABLES: u64 = 0xE_0000;
/// The first address above the legacy areas.
pub const HIGH_MEMORY: u64 = 0x10_0000;
/// RAM must end below the MMIO hole.
pub const MAX_LOW_RAM: u64 = 0xC000_0000;
/// The I/O APIC (24 pins, ID 0), which the hypervisor emulates (the backends put it at
/// its default base).
pub const IOAPIC_BASE: u64 = midgard_hv::x86::IOAPIC_BASE;
pub const IOAPIC_SIZE: u64 = 0x100;
/// The local APIC (in xAPIC mode), which the hypervisor emulates (the backends put it at
/// its default base).
pub const LAPIC_BASE: u64 = midgard_hv::x86::LAPIC_BASE;
pub const LAPIC_SIZE: u64 = 0x1000;
// The backend may use [`midgard_hv::RESERVED_BASE`] (e.g., KVM's identity map and
// real-mode TSS); it is above the local APIC.

// I/O ports.

/// The 16550A UART (eight ports).
pub const COM1_PORT: u16 = 0x3F8;
/// The ACPI PM1a event block (status and enable registers).
pub const PM1A_EVT_BLK: u16 = 0x600;
/// The ACPI PM1a control block.
pub const PM1A_CNT_BLK: u16 = 0x604;

// ISA IRQs, which map to the same I/O APIC pins.

pub const COM1_IRQ: usize = 4;
/// The ACPI SCI; it is never raised, but needs a line that no device uses.
pub const SCI_IRQ: usize = 13;

const _: () = {
    assert!(COM1_IRQ != SCI_IRQ);
    assert!(MAX_LOW_RAM <= IOAPIC_BASE);
    assert!(IOAPIC_BASE + IOAPIC_SIZE <= LAPIC_BASE);
    assert!(LAPIC_BASE + LAPIC_SIZE <= midgard_hv::RESERVED_BASE);
};
