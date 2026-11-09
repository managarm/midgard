//! The CPUID leaves that the guest sees: the host's features minus everything that we
//! (or thor) do not virtualize, plus x2APIC, the TSC-deadline timer and the KVM
//! paravirtualization leaves (for kvmclock).

use midgard_hv::CpuidEntry;
use midgard_hv::x86::cpuid::{
    LEAF1_ECX_HYPERVISOR, LEAF1_ECX_TSC_DEADLINE, LEAF1_ECX_X2APIC, LEAF1_EDX_APIC,
};
use midgard_hv::x86::{host_cpuid, xsave_compacted_size, xsave_size};

const MAX_BASIC_LEAF: u32 = 0xD;
const MAX_EXTENDED_LEAF: u32 = 0x8000_0008;
const KVM_SIGNATURE_LEAF: u32 = 0x4000_0000;
const KVM_FEATURES_LEAF: u32 = 0x4000_0001;

// KVM paravirtualization features (CPUID 0x40000001 EAX).
const KVM_FEATURE_CLOCKSOURCE: u32 = 1 << 0;
const KVM_FEATURE_NOP_IO_DELAY: u32 = 1 << 1;
const KVM_FEATURE_CLOCKSOURCE2: u32 = 1 << 3;
const KVM_FEATURE_CLOCKSOURCE_STABLE_BIT: u32 = 1 << 24;

const fn bits(list: &[u32]) -> u32 {
    let mut mask = 0;
    let mut i = 0;
    while i < list.len() {
        mask |= 1 << list[i];
        i += 1;
    }
    mask
}

// Leaf 1 ECX: DTES64, MONITOR, DS-CPL, VMX, SMX, EST, TM2, SDBG, xTPR, PDCM, PCID, DCA, OSXSAVE.
const LEAF1_ECX_CLEAR: u32 = bits(&[2, 3, 4, 5, 6, 7, 8, 11, 14, 15, 17, 18, 27]);
// Leaf 1 EDX: MCE, MTRR, MCA, PSN, DS, ACPI, HTT, TM, PBE.
const LEAF1_EDX_CLEAR: u32 = bits(&[7, 12, 14, 18, 21, 22, 28, 29, 31]);

// Leaf 7 EBX: TSC_ADJUST, SGX, INVPCID, RDT-M, MPX, RDT-A, Intel PT.
const LEAF7_EBX_CLEAR: u32 = bits(&[1, 2, 10, 12, 14, 15, 25]);
// Leaf 7 ECX: UMIP, PKU, OSPKE, WAITPKG, CET_SS, LA57, RDPID, ENQCMD, SGX_LC, PKS.
const LEAF7_ECX_CLEAR: u32 = bits(&[2, 3, 4, 5, 7, 16, 22, 29, 30, 31]);
// Leaf 7 EDX: hybrid, PCONFIG, CET_IBT, AMX, SPEC_CTRL & co., ARCH_CAPABILITIES,
// CORE_CAPABILITIES.
const LEAF7_EDX_CLEAR: u32 = bits(&[15, 18, 20, 22, 24, 25, 26, 27, 28, 29, 30, 31]);

/// XSAVE state components that we expose: x87, SSE, AVX and AVX-512.
const XCR0_SUPPORTED: u64 = 0xE7;
// Leaf 0xD subleaf 1 EAX: XSAVES, XFD.
const LEAFD1_EAX_CLEAR: u32 = bits(&[3, 4]);

// Leaf 0x80000001 ECX: SVM. EDX: RDTSCP.
const LEAF81_ECX_CLEAR: u32 = bits(&[2]);
const LEAF81_EDX_CLEAR: u32 = bits(&[27]);
// Leaf 0x80000007 EDX: invariant TSC.
const LEAF87_EDX_KEEP: u32 = 1 << 8;

/// The CPUID table of a vCPU. The bits that depend on the vCPU's state (OSXSAVE, the
/// APIC's enable bit and the XSAVE area sizes) are those of the reset state; the
/// hypervisor keeps them up to date.
pub(crate) fn table(apic_id: u32, num_cpus: u32) -> Vec<CpuidEntry> {
    let mut entries = Vec::new();
    let mut push = |function: u32, index: u32, significant_index: bool| {
        let [eax, ebx, ecx, edx] = emulate(function, index, apic_id, num_cpus);
        entries.push(CpuidEntry {
            function,
            index,
            significant_index,
            eax,
            ebx,
            ecx,
            edx,
        });
        eax
    };
    let max_basic = emulate(0, 0, apic_id, num_cpus)[0];
    for leaf in 0..=max_basic {
        match leaf {
            // Deterministic cache parameters: up to the first null cache type.
            4 => {
                for subleaf in 0..16 {
                    if push(4, subleaf, true) & 0x1F == 0 {
                        break;
                    }
                }
            }
            // Only subleaf 0 (EAX, the highest subleaf, is zero).
            7 => {
                push(7, 0, true);
            }
            0xD => {
                push(0xD, 0, true);
                push(0xD, 1, true);
                for component in 2..64 {
                    if emulate(0xD, component, apic_id, num_cpus)[0] != 0 {
                        push(0xD, component, true);
                    }
                }
            }
            _ => {
                push(leaf, 0, false);
            }
        }
    }
    push(KVM_SIGNATURE_LEAF, 0, false);
    push(KVM_FEATURES_LEAF, 0, false);
    let max_extended = push(0x8000_0000, 0, false);
    for leaf in 0x8000_0001..=max_extended {
        push(leaf, 0, false);
    }
    entries
}

/// XCR0 in the reset state: x87 only.
const RESET_XCR0: u64 = 1;

/// One leaf of the table (for a vCPU in its reset state).
fn emulate(leaf: u32, subleaf: u32, apic_id: u32, num_cpus: u32) -> [u32; 4] {
    let max_basic = host_cpuid(0, 0)[0].min(MAX_BASIC_LEAF);
    let max_extended = host_cpuid(0x8000_0000, 0)[0].min(MAX_EXTENDED_LEAF);
    match leaf {
        0 => {
            let [_, ebx, ecx, edx] = host_cpuid(0, 0);
            [max_basic, ebx, ecx, edx]
        }
        1 => {
            let [eax, ebx, ecx, edx] = host_cpuid(1, 0);
            // Initial APIC ID, logical processor count and the CLFLUSH size (kept).
            let ebx = (ebx & 0xFFFF) | (num_cpus.min(255) << 16) | (apic_id << 24);
            // OSXSAVE (cleared) reflects CR4.OSXSAVE, which is clear at reset.
            let ecx = (ecx & !LEAF1_ECX_CLEAR)
                | LEAF1_ECX_X2APIC
                | LEAF1_ECX_TSC_DEADLINE
                | LEAF1_ECX_HYPERVISOR;
            let edx = (edx & !LEAF1_EDX_CLEAR) | LEAF1_EDX_APIC;
            [eax, ebx, ecx, edx]
        }
        2 => host_cpuid(2, 0),
        4 => {
            let [eax, ebx, ecx, edx] = host_cpuid(4, subleaf);
            // One core per package, no sharing of caches between threads.
            [eax & 0x3FFF, ebx, ecx, edx]
        }
        // Always running APIC timer (ARAT); no power management.
        6 => [1 << 2, 0, 0, 0],
        7 if subleaf == 0 => {
            let [_, ebx, ecx, edx] = host_cpuid(7, 0);
            [
                0,
                ebx & !LEAF7_EBX_CLEAR,
                ecx & !LEAF7_ECX_CLEAR,
                edx & !LEAF7_EDX_CLEAR,
            ]
        }
        0xD => {
            let supported = {
                let [eax, _, _, edx] = host_cpuid(0xD, 0);
                (eax as u64 | (edx as u64) << 32) & XCR0_SUPPORTED
            };
            match subleaf {
                0 => [
                    supported as u32,
                    xsave_size(RESET_XCR0 & supported),
                    xsave_size(supported),
                    (supported >> 32) as u32,
                ],
                1 => {
                    // We do not support supervisor states (IA32_XSS is always zero).
                    let [eax, _, _, _] = host_cpuid(0xD, 1);
                    [
                        eax & !LEAFD1_EAX_CLEAR,
                        xsave_compacted_size(RESET_XCR0 & supported),
                        0,
                        0,
                    ]
                }
                n if n < 64 && supported & (1 << n) != 0 => host_cpuid(0xD, n),
                _ => [0; 4],
            }
        }
        l if l <= max_basic && matches!(l, 3 | 5 | 8..=0xC) => [0; 4],
        KVM_SIGNATURE_LEAF => {
            let signature = b"KVMKVMKVM\0\0\0";
            let word = |i: usize| u32::from_le_bytes(signature[i..i + 4].try_into().unwrap());
            [KVM_FEATURES_LEAF, word(0), word(4), word(8)]
        }
        KVM_FEATURES_LEAF => [
            KVM_FEATURE_CLOCKSOURCE
                | KVM_FEATURE_NOP_IO_DELAY
                | KVM_FEATURE_CLOCKSOURCE2
                | KVM_FEATURE_CLOCKSOURCE_STABLE_BIT,
            0,
            0,
            0,
        ],
        0x8000_0000 => {
            let [_, ebx, ecx, edx] = host_cpuid(0x8000_0000, 0);
            [max_extended, ebx, ecx, edx]
        }
        0x8000_0001 => {
            let [eax, ebx, ecx, edx] = host_cpuid(0x8000_0001, 0);
            [eax, ebx, ecx & !LEAF81_ECX_CLEAR, edx & !LEAF81_EDX_CLEAR]
        }
        0x8000_0002..=0x8000_0006 if leaf <= max_extended => host_cpuid(leaf, 0),
        0x8000_0007 if leaf <= max_extended => {
            let [_, _, _, edx] = host_cpuid(leaf, 0);
            [0, 0, 0, edx & LEAF87_EDX_KEEP]
        }
        0x8000_0008 if leaf <= max_extended => {
            let [eax, _, _, _] = host_cpuid(leaf, 0);
            [eax & 0xFFFF, 0, 0, 0]
        }
        _ => [0; 4],
    }
}
