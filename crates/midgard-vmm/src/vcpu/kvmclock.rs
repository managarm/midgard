//! kvmclock, which lets the guest compute the time from its TSC without exits. It is
//! configured per vCPU through MSRs.
//!
//! All vCPUs publish the same [`Timebase`]: one linear function of the TSC (which is
//! the host's TSC on all vCPUs) that never changes. Hence the clock is monotonic and
//! agrees across vCPUs by construction, and we set PVCLOCK_TSC_STABLE_BIT (which lets
//! Linux read it in the vDSO). It runs at the rate of the host's TSC, like a hardware
//! clock; the guest corrects for that rate itself (e.g., with NTP).

use std::time::{SystemTime, UNIX_EPOCH};

use midgard_hv::{GuestMemory, MemoryError};
use zerocopy::{Immutable, IntoBytes};

use crate::clock::rdtsc;

/// Bit 0 of the system time MSR enables kvmclock.
const SYSTEM_TIME_ENABLE: u64 = 1;
/// The time structure never changes after it was written, so its version is always the
/// same (even) number.
const TIME_INFO_VERSION: u32 = 2;
/// PVCLOCK_TSC_STABLE_BIT: the clock is monotonic across vCPUs.
const TSC_STABLE: u8 = 1 << 0;

/// Mirrors struct pvclock_vcpu_time_info.
#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct TimeInfo {
    version: u32,
    pad0: u32,
    tsc_timestamp: u64,
    system_time: u64,
    tsc_to_system_mul: u32,
    tsc_shift: i8,
    flags: u8,
    pad: [u8; 2],
}

/// Mirrors struct pvclock_wall_clock.
#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct WallClock {
    version: u32,
    sec: u32,
    nsec: u32,
}

/// Returns (shift, multiplier) such that nanoseconds = ((ticks << shift) * multiplier) >> 32
/// (or >> -shift for negative shifts). This is kvm_get_time_scale() from Linux.
fn time_scale(scaled_hz: u64, base_hz: u64) -> (i8, u32) {
    let mut shift: i32 = 0;
    let mut tps64 = base_hz;
    let mut scaled64 = scaled_hz;
    while tps64 > scaled64 * 2 || tps64 & 0xFFFF_FFFF_0000_0000 != 0 {
        tps64 >>= 1;
        shift -= 1;
    }
    let mut tps32 = tps64 as u32;
    while tps32 as u64 <= scaled64 || scaled64 & 0xFFFF_FFFF_0000_0000 != 0 {
        if scaled64 & 0xFFFF_FFFF_0000_0000 != 0 || tps32 & 0x8000_0000 != 0 {
            scaled64 >>= 1;
        } else {
            tps32 <<= 1;
        }
        shift += 1;
    }
    (shift as i8, ((scaled64 << 32) / tps32 as u64) as u32)
}

/// The guest's system time as a function of the TSC: zero at `tsc`, then counting
/// nanoseconds at the TSC's frequency.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timebase {
    tsc: u64,
    /// Converts TSC ticks to nanoseconds (see [`time_scale`]).
    shift: i8,
    mul: u32,
}

impl Timebase {
    /// Starts the system time now, given the TSC frequency.
    pub(crate) fn new(tsc_hz: u64) -> Self {
        let (shift, mul) = time_scale(1_000_000_000, tsc_hz);
        Self {
            tsc: rdtsc(),
            shift,
            mul,
        }
    }

    /// The system time at the TSC value `tsc`, computed like the guest does
    /// (pvclock_scale_delta() in Linux).
    fn system_time(&self, tsc: u64) -> u64 {
        let delta = tsc.wrapping_sub(self.tsc);
        let delta = if self.shift < 0 {
            delta >> -self.shift
        } else {
            delta << self.shift
        };
        ((delta as u128 * self.mul as u128) >> 32) as u64
    }
}

/// Per-vCPU kvmclock state.
pub(crate) struct Kvmclock {
    msr: u64,
}

impl Kvmclock {
    pub(crate) fn new() -> Self {
        Self { msr: 0 }
    }

    pub(crate) fn msr(&self) -> u64 {
        self.msr
    }

    /// Handles a write of the system time MSR: publishes `timebase` at the address that
    /// the MSR holds (if it enables kvmclock).
    pub(crate) fn set_msr(
        &mut self,
        memory: &GuestMemory,
        timebase: &Timebase,
        msr: u64,
    ) -> Result<(), MemoryError> {
        self.msr = msr;
        if msr & SYSTEM_TIME_ENABLE == 0 {
            return Ok(());
        }
        let info = TimeInfo {
            // Other vCPUs may read the structure while we write it (Linux's vDSO reads
            // that of vCPU 0), but a rewrite does not change it.
            version: TIME_INFO_VERSION,
            pad0: 0,
            tsc_timestamp: timebase.tsc,
            system_time: 0,
            tsc_to_system_mul: timebase.mul,
            tsc_shift: timebase.shift,
            flags: TSC_STABLE,
            pad: [0; 2],
        };
        memory.write_obj(msr & !SYSTEM_TIME_ENABLE, &info)
    }
}

/// Writes struct pvclock_wall_clock: the wall clock time at which the system time was zero.
pub(crate) fn write_wall_clock(
    memory: &GuestMemory,
    timebase: &Timebase,
    address: u64,
) -> Result<(), MemoryError> {
    let wall_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let boot = wall_ns.saturating_sub(timebase.system_time(rdtsc()));
    let wall = WallClock {
        version: 2,
        sec: (boot / 1_000_000_000) as u32,
        nsec: (boot % 1_000_000_000) as u32,
    };
    memory.write_obj(address, &wall)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_time_counts_nanoseconds() {
        for hz in [1_000_000_000, 2_400_000_000, 3_000_000_000, 4_999_999_000] {
            let (shift, mul) = time_scale(1_000_000_000, hz);
            let timebase = Timebase {
                tsc: 1 << 40,
                shift,
                mul,
            };
            assert_eq!(timebase.system_time(1 << 40), 0);
            // One hour of ticks; the 32-bit multiplier is exact to about 1 ns per second.
            let ns = timebase.system_time((1 << 40) + hz * 3600);
            assert!(
                ns.abs_diff(3600 * 1_000_000_000) < 3600 * 2,
                "{hz} Hz: {ns} ns"
            );
        }
    }
}
