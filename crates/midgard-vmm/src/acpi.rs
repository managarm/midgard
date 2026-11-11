//! Minimal ACPI tables: they tell Linux about the CPUs and the I/O APIC (the MADT) and
//! give it a way to power off (the FADT's PM1a control block and \_S5 in the DSDT).
//! There is no AML besides \_S5; the virtio devices are declared on the command line.

use std::mem::offset_of;

use midgard_hv::{GuestMemory, MemoryError};
use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::aml;
use crate::devices::{pm1, rtc};
use crate::platform::{IOAPIC_BASE, LAPIC_BASE, PM1A_CNT_BLK, PM1A_EVT_BLK, SCI_IRQ};

const OEM_ID: &[u8; 6] = b"MIDGRD";
const OEM_TABLE_ID: &[u8; 8] = b"MIDGARD ";

// MADT entry types.
const MADT_LOCAL_APIC: u8 = 0;
const MADT_IOAPIC: u8 = 1;
const MADT_INTERRUPT_OVERRIDE: u8 = 2;
const MADT_LOCAL_APIC_NMI: u8 = 4;
/// Processor Local APIC flags: the processor is enabled.
const MADT_ENABLED: u32 = 1;
/// MPS INTI flags: active high, level-triggered.
const INTI_ACTIVE_HIGH_LEVEL: u16 = 0b01 | 0b11 << 2;

// FADT flags.
const FADT_WBINVD: u32 = 1 << 0;
/// No fixed-feature power button.
const FADT_PWR_BUTTON: u32 = 1 << 4;
/// No fixed-feature sleep button.
const FADT_SLP_BUTTON: u32 = 1 << 5;
/// IA-PC boot architecture flags: no VGA (and, since bit 1 is clear, no 8042).
const IAPC_BOOT_ARCH_NO_VGA: u16 = 1 << 2;

fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)))
}

/// The header of a system description table.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct SdtHeader {
    signature: [u8; 4],
    length: U32,
    revision: u8,
    checksum: u8,
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    oem_revision: U32,
    creator_id: [u8; 4],
    creator_revision: U32,
}

/// The Root System Description Pointer (ACPI 2.0+).
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Rsdp {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt_address: U32,
    length: U32,
    xsdt_address: U64,
    extended_checksum: u8,
    reserved: [u8; 3],
}

/// The Firmware ACPI Control Structure.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Facs {
    signature: [u8; 4],
    length: U32,
    hardware_signature: U32,
    firmware_waking_vector: U32,
    global_lock: U32,
    flags: U32,
    x_firmware_waking_vector: U64,
    version: u8,
    reserved: [u8; 3],
    ospm_flags: U32,
    reserved2: [u8; 24],
}

/// A Generic Address Structure.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Gas {
    address_space_id: u8,
    register_bit_width: u8,
    register_bit_offset: u8,
    access_size: u8,
    address: U64,
}

/// The FADT after its header (ACPI 6.5 layout).
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Fadt {
    firmware_ctrl: U32,
    dsdt: U32,
    reserved: u8,
    preferred_pm_profile: u8,
    sci_int: U16,
    smi_cmd: U32,
    acpi_enable: u8,
    acpi_disable: u8,
    s4bios_req: u8,
    pstate_cnt: u8,
    pm1a_evt_blk: U32,
    pm1b_evt_blk: U32,
    pm1a_cnt_blk: U32,
    pm1b_cnt_blk: U32,
    pm2_cnt_blk: U32,
    pm_tmr_blk: U32,
    gpe0_blk: U32,
    gpe1_blk: U32,
    pm1_evt_len: u8,
    pm1_cnt_len: u8,
    pm2_cnt_len: u8,
    pm_tmr_len: u8,
    gpe0_blk_len: u8,
    gpe1_blk_len: u8,
    gpe1_base: u8,
    cst_cnt: u8,
    p_lvl2_lat: U16,
    p_lvl3_lat: U16,
    flush_size: U16,
    flush_stride: U16,
    duty_offset: u8,
    duty_width: u8,
    day_alrm: u8,
    mon_alrm: u8,
    century: u8,
    iapc_boot_arch: U16,
    reserved2: u8,
    flags: U32,
    reset_reg: Gas,
    reset_value: u8,
    arm_boot_arch: U16,
    fadt_minor_version: u8,
    x_firmware_ctrl: U64,
    x_dsdt: U64,
    x_pm1a_evt_blk: Gas,
    x_pm1b_evt_blk: Gas,
    x_pm1a_cnt_blk: Gas,
    x_pm1b_cnt_blk: Gas,
    x_pm2_cnt_blk: Gas,
    x_pm_tmr_blk: Gas,
    x_gpe0_blk: Gas,
    x_gpe1_blk: Gas,
    sleep_control_reg: Gas,
    sleep_status_reg: Gas,
    hypervisor_vendor_identity: U64,
}

/// The MADT's fields after its header (the interrupt controller structures follow).
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Madt {
    local_apic_address: U32,
    flags: U32,
}

/// A MADT entry: Processor Local APIC.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct MadtLocalApic {
    kind: u8,
    length: u8,
    processor_uid: u8,
    apic_id: u8,
    flags: U32,
}

/// A MADT entry: I/O APIC.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct MadtIoApic {
    kind: u8,
    length: u8,
    id: u8,
    reserved: u8,
    address: U32,
    gsi_base: U32,
}

/// A MADT entry: Interrupt Source Override.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct MadtInterruptOverride {
    kind: u8,
    length: u8,
    bus: u8,
    source: u8,
    gsi: U32,
    flags: U16,
}

/// A MADT entry: Local APIC NMI.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct MadtLocalApicNmi {
    kind: u8,
    length: u8,
    processor_uid: u8,
    flags: U16,
    lint: u8,
}

const _: () = {
    assert!(size_of::<SdtHeader>() == 36);
    assert!(size_of::<Rsdp>() == 36);
    assert!(size_of::<Facs>() == 64);
    assert!(size_of::<SdtHeader>() + size_of::<Fadt>() == 276);
    assert!(size_of::<SdtHeader>() + offset_of!(Fadt, sci_int) == 46);
    assert!(size_of::<SdtHeader>() + offset_of!(Fadt, iapc_boot_arch) == 109);
    assert!(size_of::<SdtHeader>() + offset_of!(Fadt, flags) == 112);
    assert!(size_of::<SdtHeader>() + offset_of!(Fadt, fadt_minor_version) == 131);
    assert!(size_of::<MadtLocalApic>() == 8);
    assert!(size_of::<MadtIoApic>() == 12);
    assert!(size_of::<MadtInterruptOverride>() == 10);
    assert!(size_of::<MadtLocalApicNmi>() == 6);
};

/// The length field of a MADT entry.
const fn entry_length<T>() -> u8 {
    size_of::<T>() as u8
}

/// A system description table: the common header, then `body`.
fn sdt(signature: &[u8; 4], revision: u8, body: &[u8]) -> Vec<u8> {
    let header = SdtHeader {
        signature: *signature,
        length: ((size_of::<SdtHeader>() + body.len()) as u32).into(),
        revision,
        checksum: 0,
        oem_id: *OEM_ID,
        oem_table_id: *OEM_TABLE_ID,
        oem_revision: 1.into(),
        creator_id: *b"MDGD",
        creator_revision: 1.into(),
    };
    let mut table = header.as_bytes().to_vec();
    table.extend_from_slice(body);
    table[offset_of!(SdtHeader, checksum)] = checksum(&table);
    table
}

/// The tables, laid out one after the other at their guest-physical address.
struct Layout {
    base: u64,
    bytes: Vec<u8>,
}

impl Layout {
    fn add(&mut self, table: &[u8], align: usize) -> u64 {
        self.bytes
            .resize(self.bytes.len().next_multiple_of(align), 0);
        let address = self.base + self.bytes.len() as u64;
        self.bytes.extend_from_slice(table);
        address
    }
}

/// The DSDT: \_S5 only.
fn dsdt() -> Vec<u8> {
    let s5 = u64::from(pm1::S5_SLP_TYP);
    let aml = aml::term_list(&[
        // SLP_TYPa and SLP_TYPb, then two reserved values.
        aml::name(
            "_S5",
            aml::package(&[
                aml::integer(s5),
                aml::integer(s5),
                aml::integer(0),
                aml::integer(0),
            ]),
        ),
    ]);
    sdt(b"DSDT", 2, &aml)
}

/// The FACS (the FADT must point to one on hardware with fixed ACPI registers).
fn facs() -> Facs {
    let mut facs = Facs::new_zeroed();
    facs.signature = *b"FACS";
    facs.length.set(size_of::<Facs>() as u32);
    facs.version = 2;
    facs
}

/// The FADT (the 32-bit fields suffice below 4 GiB).
fn fadt(facs: u64, dsdt: u64) -> Vec<u8> {
    let mut fadt = Fadt::new_zeroed();
    fadt.firmware_ctrl.set(facs as u32);
    fadt.dsdt.set(dsdt as u32);
    fadt.sci_int.set(SCI_IRQ as u16);
    // No SMI_CMD: the machine is always in ACPI mode.
    fadt.pm1a_evt_blk.set(PM1A_EVT_BLK as u32);
    fadt.pm1a_cnt_blk.set(PM1A_CNT_BLK as u32);
    fadt.pm1_evt_len = pm1::EVT_LEN as u8;
    fadt.pm1_cnt_len = pm1::CNT_LEN as u8;
    // C2 and C3 are not supported.
    fadt.p_lvl2_lat.set(101);
    fadt.p_lvl3_lat.set(1001);
    fadt.century = rtc::REG_CENTURY;
    fadt.iapc_boot_arch.set(IAPC_BOOT_ARCH_NO_VGA);
    fadt.flags
        .set(FADT_WBINVD | FADT_PWR_BUTTON | FADT_SLP_BUTTON);
    fadt.fadt_minor_version = 5;
    sdt(b"FACP", 6, fadt.as_bytes())
}

/// The MADT: the local APICs, the I/O APIC (which keeps its reset ID 0), the SCI's
/// polarity and trigger mode, and LINT1 as NMI. ISA IRQs map to the same I/O APIC pins
/// (edge-triggered, active high), which is ACPI's default.
fn madt(num_cpus: u32) -> Vec<u8> {
    let madt = Madt {
        local_apic_address: (LAPIC_BASE as u32).into(),
        flags: 0.into(),
    };
    let mut body = madt.as_bytes().to_vec();
    for cpu in 0..num_cpus {
        let local_apic = MadtLocalApic {
            kind: MADT_LOCAL_APIC,
            length: entry_length::<MadtLocalApic>(),
            processor_uid: cpu as u8,
            apic_id: cpu as u8,
            flags: MADT_ENABLED.into(),
        };
        body.extend_from_slice(local_apic.as_bytes());
    }
    let ioapic = MadtIoApic {
        kind: MADT_IOAPIC,
        length: entry_length::<MadtIoApic>(),
        id: 0,
        reserved: 0,
        address: (IOAPIC_BASE as u32).into(),
        gsi_base: 0.into(),
    };
    body.extend_from_slice(ioapic.as_bytes());
    // The SCI is never raised; it is active high, such that the low line is idle.
    let sci = MadtInterruptOverride {
        kind: MADT_INTERRUPT_OVERRIDE,
        length: entry_length::<MadtInterruptOverride>(),
        bus: 0,
        source: SCI_IRQ as u8,
        gsi: (SCI_IRQ as u32).into(),
        flags: INTI_ACTIVE_HIGH_LEVEL.into(),
    };
    body.extend_from_slice(sci.as_bytes());
    // All processors, flags conform to the bus, LINT1.
    let nmi = MadtLocalApicNmi {
        kind: MADT_LOCAL_APIC_NMI,
        length: entry_length::<MadtLocalApicNmi>(),
        processor_uid: 0xFF,
        flags: 0.into(),
        lint: 1,
    };
    body.extend_from_slice(nmi.as_bytes());
    sdt(b"APIC", 5, &body)
}

/// Writes the tables to `address`, with the RSDP first (16-byte aligned, such that Linux
/// also finds it when it scans the BIOS area).
pub(crate) fn write(memory: &GuestMemory, address: u64, num_cpus: u32) -> Result<(), MemoryError> {
    let mut layout = Layout {
        base: address,
        bytes: vec![0u8; size_of::<Rsdp>()],
    };
    let dsdt = layout.add(&dsdt(), 8);
    let facs = layout.add(facs().as_bytes(), 64);
    let fadt = layout.add(&fadt(facs, dsdt), 8);
    let madt = layout.add(&madt(num_cpus), 8);
    let entries = [U64::new(fadt), U64::new(madt)];
    let xsdt = layout.add(&sdt(b"XSDT", 1, entries.as_bytes()), 8);

    let mut rsdp = Rsdp {
        signature: *b"RSD PTR ",
        checksum: 0,
        oem_id: *OEM_ID,
        // ACPI 2.0+: there is an XSDT (and no RSDT).
        revision: 2,
        rsdt_address: 0.into(),
        length: (size_of::<Rsdp>() as u32).into(),
        xsdt_address: xsdt.into(),
        extended_checksum: 0,
        reserved: [0; 3],
    };
    // The checksum covers the ACPI 1.0 part; the extended checksum the whole structure.
    rsdp.checksum = checksum(&rsdp.as_bytes()[..offset_of!(Rsdp, length)]);
    rsdp.extended_checksum = checksum(rsdp.as_bytes());
    layout.bytes[..size_of::<Rsdp>()].copy_from_slice(rsdp.as_bytes());

    memory.write(address, &layout.bytes)
}
