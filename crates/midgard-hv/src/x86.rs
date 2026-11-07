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

/// CPUID feature bits.
pub mod cpuid {
    pub const LEAF1_ECX_X2APIC: u32 = 1 << 21;
    pub const LEAF1_ECX_TSC_DEADLINE: u32 = 1 << 24;
    pub const LEAF1_ECX_HYPERVISOR: u32 = 1 << 31;
    pub const LEAF1_EDX_APIC: u32 = 1 << 9;
}
