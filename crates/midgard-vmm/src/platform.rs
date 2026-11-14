//! The guest's machine: where midgard puts things in the guest-physical address space,
//! which I/O ports and which interrupt lines its devices use. Keeping them in one place
//! makes it easy to see that nothing overlaps.

use crate::{pci, virtio};

// Guest-physical memory. RAM starts at zero; the MMIO hole is at the top of the 4 GiB.

/// The legacy video and BIOS areas (reserved in the E820 map), up to [`HIGH_MEMORY`].
pub(crate) const LEGACY_HOLE: u64 = 0xA_0000;
/// The ACPI tables (the RSDP first) are in the BIOS area, where Linux searches for the
/// RSDP; the zero page also points to them.
pub(crate) const ACPI_TABLES: u64 = 0xE_0000;
/// The first address above the legacy areas.
pub(crate) const HIGH_MEMORY: u64 = 0x10_0000;
/// RAM must end below the MMIO hole.
pub(crate) const MAX_LOW_RAM: u64 = 0xC000_0000;
/// The memory window of the PCI host bridge, where the devices' BARs are (see
/// [`virtio_slot`]).
pub(crate) const PCI_MMIO_BASE: u64 = 0xC000_0000;
pub(crate) const PCI_MMIO_SIZE: u64 = 0x3000_0000;
/// The PCI bus' configuration space (ECAM, [`pci::ECAM_SIZE`] bytes).
pub(crate) const PCI_ECAM_BASE: u64 = 0xF000_0000;
/// The I/O APIC (24 pins, ID 0), which the hypervisor emulates (the backends put it at
/// its default base).
pub(crate) const IOAPIC_BASE: u64 = midgard_hv::x86::IOAPIC_BASE;
pub(crate) const IOAPIC_SIZE: u64 = 0x100;
/// The local APIC (in xAPIC mode), which the hypervisor emulates (the backends put it at
/// its default base).
pub(crate) const LAPIC_BASE: u64 = midgard_hv::x86::LAPIC_BASE;
pub(crate) const LAPIC_SIZE: u64 = 0x1000;
// The backend may use [`midgard_hv::RESERVED_BASE`] (e.g., KVM's identity map and
// real-mode TSS); it is above the local APIC.

// I/O ports.

/// The 16550A UART (eight ports).
pub(crate) const COM1_PORT: u16 = 0x3F8;
/// The CMOS RTC (an index and a data port).
pub(crate) const RTC_PORT: u16 = 0x70;
/// The ACPI PM1a event block (status and enable registers).
pub(crate) const PM1A_EVT_BLK: u16 = 0x600;
/// The ACPI PM1a control block.
pub(crate) const PM1A_CNT_BLK: u16 = 0x604;

// ISA IRQs, which map to the same I/O APIC pins.

pub(crate) const COM1_IRQ: usize = 4;
/// The ACPI SCI; it is never raised, but needs a line that no device uses.
pub(crate) const SCI_IRQ: usize = 13;
// PCI slots.

/// The slot of the first virtio device; the others follow (slot 0 is the host bridge).
pub(crate) const FIRST_VIRTIO_SLOT: usize = 1;
/// The most virtio devices: one in each of the other slots.
pub(crate) const MAX_VIRTIO_DEVICES: usize = pci::NUM_SLOTS - FIRST_VIRTIO_SLOT;

const _: () = {
    assert!(COM1_IRQ != SCI_IRQ);
    assert!(MAX_LOW_RAM <= PCI_MMIO_BASE);
    // The BARs are naturally aligned and fit into the window.
    assert!(PCI_MMIO_BASE.is_multiple_of(virtio::BAR_SIZE));
    assert!(MAX_VIRTIO_DEVICES as u64 * virtio::BAR_SIZE <= PCI_MMIO_SIZE);
    assert!(PCI_MMIO_BASE + PCI_MMIO_SIZE <= PCI_ECAM_BASE);
    assert!(PCI_ECAM_BASE + pci::ECAM_SIZE <= IOAPIC_BASE);
    assert!(IOAPIC_BASE + IOAPIC_SIZE <= LAPIC_BASE);
    assert!(LAPIC_BASE + LAPIC_SIZE <= midgard_hv::RESERVED_BASE);
};

/// The PCI slot and the BAR's address of the virtio device `index` (in the order of the
/// [`virtio::DeviceList`]).
pub(crate) fn virtio_slot(index: usize) -> (usize, u64) {
    (
        FIRST_VIRTIO_SLOT + index,
        PCI_MMIO_BASE + index as u64 * virtio::BAR_SIZE,
    )
}

/// The most vCPUs: the guest kernel's NR_CPUS (APIC IDs are also 8 bits in the MADT).
pub(crate) const MAX_CPUS: u32 = 64;
