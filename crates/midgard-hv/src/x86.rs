//! Architectural constants of x86-64: register bits, MSRs, CPUID feature bits and page
//! table entries.

/// Bit 1 of RFLAGS is reserved and always set.
pub const RFLAGS_FIXED: u64 = 1 << 1;
pub const RFLAGS_IF: u64 = 1 << 9;

pub const CR0_PE: u64 = 1 << 0;
pub const CR0_MP: u64 = 1 << 1;
pub const CR0_ET: u64 = 1 << 4;
pub const CR0_NE: u64 = 1 << 5;
pub const CR0_PG: u64 = 1 << 31;

pub const CR4_PSE: u64 = 1 << 4;
pub const CR4_PAE: u64 = 1 << 5;
pub const CR4_LA57: u64 = 1 << 12;
pub const CR4_OSXSAVE: u64 = 1 << 18;

pub const EFER_LME: u64 = 1 << 8;
pub const EFER_LMA: u64 = 1 << 10;

pub const PTE_PRESENT: u64 = 1 << 0;
pub const PTE_WRITABLE: u64 = 1 << 1;
/// A large page (in a PDPT or page directory).
pub const PTE_LARGE: u64 = 1 << 7;

/// The I/O APIC's default base address (where the backends put it).
pub const IOAPIC_BASE: u64 = 0xFEC0_0000;
/// The local APIC's default base address (where the backends put it).
pub const LAPIC_BASE: u64 = 0xFEE0_0000;

/// Model-specific registers.
pub mod msr {
    pub const TSC: u32 = 0x10;
    pub const APIC_BASE: u32 = 0x1B;
    pub const FEATURE_CONTROL: u32 = 0x3A;
    pub const BIOS_SIGN_ID: u32 = 0x8B;
    pub const MISC_FEATURES_ENABLES: u32 = 0x140;
    pub const SYSENTER_CS: u32 = 0x174;
    pub const SYSENTER_ESP: u32 = 0x175;
    pub const SYSENTER_EIP: u32 = 0x176;
    pub const MISC_ENABLE: u32 = 0x1A0;
    pub const PAT: u32 = 0x277;
    pub const TSC_DEADLINE: u32 = 0x6E0;
    /// The x2APIC registers (256 MSRs).
    pub const X2APIC_FIRST: u32 = 0x800;
    pub const EFER: u32 = 0xC000_0080;
    pub const STAR: u32 = 0xC000_0081;
    pub const LSTAR: u32 = 0xC000_0082;
    pub const CSTAR: u32 = 0xC000_0083;
    pub const SFMASK: u32 = 0xC000_0084;
    pub const FS_BASE: u32 = 0xC000_0100;
    pub const GS_BASE: u32 = 0xC000_0101;
    pub const KERNEL_GS_BASE: u32 = 0xC000_0102;

    // KVM's paravirtualized clock.
    pub const KVM_WALL_CLOCK: u32 = 0x11;
    pub const KVM_SYSTEM_TIME: u32 = 0x12;
    pub const KVM_WALL_CLOCK_NEW: u32 = 0x4b56_4d00;
    pub const KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;

    // IA32_APIC_BASE bits.
    pub const APIC_BASE_BSP: u64 = 1 << 8;
    /// x2APIC mode (with [`APIC_BASE_ENABLE`]).
    pub const APIC_BASE_EXTD: u64 = 1 << 10;
    pub const APIC_BASE_ENABLE: u64 = 1 << 11;
}

/// The format of message signaled interrupts (to the local APICs, without interrupt
/// remapping): the address selects the destination, the data the vector and how it is
/// delivered.
pub mod msi {
    /// The address window of MSIs: bits 31:20 of the address (bits 63:32 are zero).
    pub const ADDRESS_BASE: u64 = 0xFEE0_0000;
    pub const ADDRESS_MASK: u64 = 0xFFFF_FFFF_FFF0_0000;
    /// The destination that addresses all local APICs.
    pub const BROADCAST: u8 = 0xFF;
    /// The local APIC ID (or, in logical mode, the logical destination).
    pub const ADDRESS_DESTINATION_SHIFT: u32 = 12;
    pub const ADDRESS_LOGICAL: u64 = 1 << 2;
    pub const DATA_VECTOR: u32 = 0xFF;
    pub const DATA_DELIVERY_MODE: u32 = 7 << 8;
    pub const DATA_LOWEST_PRIORITY: u32 = 1 << 8;
    pub const DATA_ASSERT: u32 = 1 << 14;
    pub const DATA_LEVEL: u32 = 1 << 15;
}

/// CPUID feature bits.
pub mod cpuid {
    pub const LEAF1_ECX_X2APIC: u32 = 1 << 21;
    pub const LEAF1_ECX_TSC_DEADLINE: u32 = 1 << 24;
    pub const LEAF1_ECX_OSXSAVE: u32 = 1 << 27;
    pub const LEAF1_ECX_HYPERVISOR: u32 = 1 << 31;
    pub const LEAF1_EDX_APIC: u32 = 1 << 9;
}

/// CPUID on the host.
#[cfg(target_arch = "x86_64")]
pub fn host_cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
    #[allow(unused_unsafe)]
    // SAFETY: CPUID is always available on x86_64.
    let r = unsafe { core::arch::x86_64::__cpuid_count(leaf, subleaf) };
    [r.eax, r.ebx, r.ecx, r.edx]
}

#[cfg(not(target_arch = "x86_64"))]
pub fn host_cpuid(_leaf: u32, _subleaf: u32) -> [u32; 4] {
    [0; 4]
}

/// The size of the (standard format) XSAVE area for the components in `xcr0`.
pub fn xsave_size(xcr0: u64) -> u32 {
    // Legacy region and XSAVE header.
    let mut size = 576;
    for component in 2..64 {
        if xcr0 & (1 << component) != 0 {
            let [component_size, offset, _, _] = host_cpuid(0xD, component);
            size = size.max(offset + component_size);
        }
    }
    size
}

/// The size of the compacted format XSAVE area (XSAVEC) for the components in `xcr0`.
pub fn xsave_compacted_size(xcr0: u64) -> u32 {
    let mut size: u32 = 576;
    for component in 2..64 {
        if xcr0 & (1 << component) != 0 {
            let [component_size, _, flags, _] = host_cpuid(0xD, component);
            // Bit 1: the component is 64-byte aligned in the compacted format.
            if flags & 2 != 0 {
                size = size.next_multiple_of(64);
            }
            size += component_size;
        }
    }
    size
}
