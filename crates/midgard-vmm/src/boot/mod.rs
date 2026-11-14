//! Direct boot of a Linux bzImage through the 64-bit boot protocol
//! (Documentation/arch/x86/boot.rst): we load the protected-mode kernel, fill in the
//! zero page (struct boot_params) and enter startup_64 in long mode.

use std::mem::offset_of;

use anyhow::{Context, Result, bail};
use midgard_hv::x86::{
    CR0_ET, CR0_MP, CR0_NE, CR0_PE, CR0_PG, CR4_PAE, EFER_LMA, EFER_LME, PTE_LARGE, PTE_PRESENT,
    PTE_WRITABLE, RFLAGS_FIXED,
};
use midgard_hv::{DescriptorTable, GuestMemory, MemoryError, Regs, Segment};
use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::pci;
use crate::platform::{ACPI_TABLES, HIGH_MEMORY, LEGACY_HOLE, PCI_ECAM_BASE};

pub(crate) mod initramfs;

// Guest-physical layout of the boot structures (all below 1 MiB).
const GDT_ADDRESS: u64 = 0x500;
const ZERO_PAGE_ADDRESS: u64 = 0x7000;
const PML4_ADDRESS: u64 = 0x9000;
const PDPT_ADDRESS: u64 = 0xA000;
/// Four page directories that identity map the first 4 GiB with 2 MiB pages.
const PD_ADDRESS: u64 = 0xB000;
const CMDLINE_ADDRESS: u64 = 0x20000;
const CMDLINE_MAX: usize = 0x10000;
/// Where we load the protected-mode kernel (code32_start). The decompressor relocates
/// itself as needed.
const KERNEL_ADDRESS: u64 = HIGH_MEMORY;

const LOADFLAGS_LOADED_HIGH: u8 = 1 << 0;
const XLF_KERNEL_64: u16 = 1 << 0;
/// The minimum boot protocol version: 2.12 has the 64-bit entry point.
const MIN_VERSION: u16 = 0x20C;
/// type_of_loader: "undefined".
const LOADER_UNDEFINED: u8 = 0xFF;

const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;

// Selectors of the boot GDT (__BOOT_CS and __BOOT_DS in Linux).
const BOOT_CS: u16 = 0x10;
const BOOT_DS: u16 = 0x18;

/// struct setup_header, which is at [`SETUP_HEADER`] in the bzImage and in the zero page.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct SetupHeader {
    setup_sects: u8,
    root_flags: U16,
    syssize: U32,
    ram_size: U16,
    vid_mode: U16,
    root_dev: U16,
    boot_flag: U16,
    /// A short jump over the header (its second byte is the header's length after
    /// `header`).
    jump: U16,
    header: U32,
    version: U16,
    realmode_swtch: U32,
    start_sys_seg: U16,
    kernel_version: U16,
    type_of_loader: u8,
    loadflags: u8,
    setup_move_size: U16,
    code32_start: U32,
    ramdisk_image: U32,
    ramdisk_size: U32,
    bootsect_kludge: U32,
    heap_end_ptr: U16,
    ext_loader_ver: u8,
    ext_loader_type: u8,
    cmd_line_ptr: U32,
    initrd_addr_max: U32,
    kernel_alignment: U32,
    relocatable_kernel: u8,
    min_alignment: u8,
    xloadflags: U16,
    cmdline_size: U32,
    hardware_subarch: U32,
    hardware_subarch_data: U64,
    payload_offset: U32,
    payload_length: U32,
    setup_data: U64,
    pref_address: U64,
    init_size: U32,
    handover_offset: U32,
    kernel_info_offset: U32,
}

/// struct boot_e820_entry.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct E820Entry {
    address: U64,
    size: U64,
    kind: U32,
}

/// struct boot_params (the zero page), with the fields that we fill in.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct BootParams {
    _reserved0: [u8; 0x70],
    acpi_rsdp_addr: U64,
    _reserved1: [u8; 0x170],
    e820_entries: u8,
    _reserved2: [u8; 0x8],
    hdr: SetupHeader,
    _reserved3: [u8; 0x64],
    e820_table: [E820Entry; 128],
    _reserved4: [u8; 0x330],
}

/// The offset of the setup header (in the bzImage and in the zero page).
const SETUP_HEADER: usize = offset_of!(BootParams, hdr);

const _: () = {
    assert!(size_of::<BootParams>() == 0x1000);
    assert!(offset_of!(BootParams, acpi_rsdp_addr) == 0x70);
    assert!(offset_of!(BootParams, e820_entries) == 0x1E8);
    assert!(SETUP_HEADER == 0x1F1);
    assert!(offset_of!(BootParams, e820_table) == 0x2D0);
    assert!(SETUP_HEADER + offset_of!(SetupHeader, boot_flag) == 0x1FE);
    assert!(SETUP_HEADER + offset_of!(SetupHeader, header) == 0x202);
    assert!(SETUP_HEADER + offset_of!(SetupHeader, type_of_loader) == 0x210);
    assert!(SETUP_HEADER + offset_of!(SetupHeader, cmd_line_ptr) == 0x228);
    assert!(SETUP_HEADER + offset_of!(SetupHeader, xloadflags) == 0x236);
    assert!(SETUP_HEADER + offset_of!(SetupHeader, init_size) == 0x260);
};

/// The register state in which the guest starts.
pub(crate) struct EntryState {
    pub rip: u64,
    pub rsi: u64,
    pub cr3: u64,
}

/// Loads the kernel (and initrd) and writes all structures that the guest needs at entry.
pub(crate) fn load(
    memory: &GuestMemory,
    kernel: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &str,
) -> Result<EntryState> {
    let header = kernel
        .get(SETUP_HEADER..)
        .and_then(|bytes| SetupHeader::ref_from_prefix(bytes).ok())
        .map(|(header, _)| header);
    let Some(header) = header.filter(|header| header.boot_flag.get() == 0xAA55) else {
        bail!("kernel is not a bzImage (no boot flag)");
    };
    if header.header.get() != u32::from_le_bytes(*b"HdrS") {
        bail!("kernel is not a bzImage (no HdrS header)");
    }
    let version = header.version.get();
    if version < MIN_VERSION {
        bail!("boot protocol {version:#x} is too old (need 2.12 for the 64-bit entry point)");
    }
    if header.xloadflags.get() & XLF_KERNEL_64 == 0 {
        bail!("kernel has no 64-bit entry point");
    }
    if header.loadflags & LOADFLAGS_LOADED_HIGH == 0 {
        bail!("kernel is not a bzImage (zImages are not supported)");
    }

    let setup_sects = match header.setup_sects {
        0 => 4,
        sects => sects as usize,
    };
    let kernel_offset = (setup_sects + 1) * 512;
    if kernel_offset >= kernel.len() {
        bail!("kernel is truncated");
    }
    let protected_mode = &kernel[kernel_offset..];
    let init_size = header.init_size.get() as u64;
    // The decompressor moves itself up to (at least) LOAD_PHYSICAL_ADDR (usually 16 MiB);
    // make sure that there is some room for that.
    let kernel_end = KERNEL_ADDRESS + (protected_mode.len() as u64).max(init_size);
    if kernel_end + 0x100_0000 > memory.size() {
        bail!("not enough guest memory for the kernel (init_size {init_size:#x})");
    }
    memory
        .write(KERNEL_ADDRESS, protected_mode)
        .context("failed to load the kernel")?;

    let mut zero_page = BootParams::new_zeroed();
    // The kernel's setup header, as long as the kernel says it is (it ends after
    // `header` plus the offset of the jump).
    let header_length = (offset_of!(SetupHeader, header) + (header.jump.get() >> 8) as usize)
        .min(size_of::<SetupHeader>());
    zero_page.hdr.as_mut_bytes()[..header_length]
        .copy_from_slice(&header.as_bytes()[..header_length]);
    let hdr = &mut zero_page.hdr;
    hdr.type_of_loader = LOADER_UNDEFINED;
    hdr.code32_start.set(KERNEL_ADDRESS as u32);

    let cmdline_size = (header.cmdline_size.get() as usize).min(CMDLINE_MAX - 1);
    if cmdline.len() > cmdline_size {
        bail!("kernel command line is longer than {cmdline_size} bytes");
    }
    let mut cmdline_bytes = cmdline.as_bytes().to_vec();
    cmdline_bytes.push(0);
    memory
        .write(CMDLINE_ADDRESS, &cmdline_bytes)
        .context("failed to write the command line")?;
    hdr.cmd_line_ptr.set(CMDLINE_ADDRESS as u32);

    if let Some(initrd) = initrd {
        let addr_max = header.initrd_addr_max.get() as u64;
        let top = memory.size().min(addr_max + 1);
        let size = initrd.len() as u64;
        let address = top
            .checked_sub(size)
            .map(|address| address & !0xFFF)
            .filter(|&address| address >= kernel_end)
            .context("not enough guest memory for the initrd")?;
        memory
            .write(address, initrd)
            .context("failed to load the initrd")?;
        hdr.ramdisk_image.set(address as u32);
        hdr.ramdisk_size.set(size as u32);
    }

    let e820 = [
        (0, LEGACY_HOLE, E820_RAM),
        (LEGACY_HOLE, HIGH_MEMORY - LEGACY_HOLE, E820_RESERVED),
        (HIGH_MEMORY, memory.size() - HIGH_MEMORY, E820_RAM),
        (PCI_ECAM_BASE, pci::ECAM_SIZE, E820_RESERVED),
    ];
    for (entry, (address, size, kind)) in zero_page.e820_table.iter_mut().zip(e820) {
        *entry = E820Entry {
            address: address.into(),
            size: size.into(),
            kind: kind.into(),
        };
    }
    zero_page.e820_entries = e820.len() as u8;
    zero_page.acpi_rsdp_addr.set(ACPI_TABLES);
    memory
        .write_obj(ZERO_PAGE_ADDRESS, &zero_page)
        .context("failed to write the zero page")?;

    write_page_tables(memory).context("failed to write the page tables")?;

    // Null descriptors, then __BOOT_CS (64-bit code) and __BOOT_DS (data).
    let gdt = [0, 0, 0x00AF_9B00_0000_FFFF, 0x00CF_9300_0000_FFFF].map(U64::new);
    memory
        .write_obj(GDT_ADDRESS, &gdt)
        .context("failed to write the GDT")?;

    Ok(EntryState {
        // startup_64 is 0x200 bytes into the protected-mode kernel.
        rip: KERNEL_ADDRESS + 0x200,
        rsi: ZERO_PAGE_ADDRESS,
        cr3: PML4_ADDRESS,
    })
}

/// Identity maps the first 4 GiB: the protocol requires that the kernel, the zero page
/// and the command line are mapped.
fn write_page_tables(memory: &GuestMemory) -> Result<(), MemoryError> {
    memory.write_u64(PML4_ADDRESS, PDPT_ADDRESS | PTE_PRESENT | PTE_WRITABLE)?;
    for i in 0..4u64 {
        let pd = PD_ADDRESS + i * 0x1000;
        memory.write_u64(PDPT_ADDRESS + i * 8, pd | PTE_PRESENT | PTE_WRITABLE)?;
        for j in 0..512u64 {
            let address = (i << 30) | (j << 21);
            // 2 MiB pages.
            let flags = PTE_PRESENT | PTE_WRITABLE | PTE_LARGE;
            memory.write_u64(pd + j * 8, address | flags)?;
        }
    }
    Ok(())
}

fn segment(selector: u16, type_: u8, s: u8, l: u8, db: u8) -> Segment {
    Segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector,
        type_,
        present: 1,
        dpl: 0,
        db,
        s,
        l,
        g: 1,
        avl: 0,
        unusable: 0,
    }
}

/// The initial register state of the boot CPU: long mode with paging, the boot GDT
/// and RSI pointing to the zero page.
pub(crate) fn entry_registers(entry: &EntryState) -> Regs {
    let mut regs = Regs::default();
    regs.basic.rip = entry.rip;
    regs.basic.rsi = entry.rsi;
    // Interrupts are disabled.
    regs.basic.rflags = RFLAGS_FIXED;

    // Code: execute/read, accessed. Data: read/write, accessed.
    regs.cs = segment(BOOT_CS, 0xB, 1, 1, 0);
    let data = segment(BOOT_DS, 0x3, 1, 0, 1);
    regs.ds = data;
    regs.es = data;
    regs.fs = data;
    regs.gs = data;
    regs.ss = data;
    // VM entry requires a busy 64-bit TSS; Linux loads its own TR before it needs one.
    regs.tr = Segment {
        base: 0,
        limit: 0x67,
        selector: 0,
        type_: 0xB,
        present: 1,
        dpl: 0,
        db: 0,
        s: 0,
        l: 0,
        g: 0,
        avl: 0,
        unusable: 0,
    };
    regs.ldt = Segment {
        base: 0,
        limit: 0xFFFF,
        selector: 0,
        type_: 0x2,
        present: 1,
        dpl: 0,
        db: 0,
        s: 0,
        l: 0,
        g: 0,
        avl: 0,
        unusable: 1,
    };
    regs.gdt = DescriptorTable {
        base: GDT_ADDRESS,
        limit: 4 * 8 - 1,
    };
    regs.idt = DescriptorTable { base: 0, limit: 0 };

    regs.cr0 = CR0_PE | CR0_MP | CR0_ET | CR0_NE | CR0_PG;
    regs.cr3 = entry.cr3;
    regs.cr4 = CR4_PAE;
    regs.efer = EFER_LME | EFER_LMA;
    regs
}
