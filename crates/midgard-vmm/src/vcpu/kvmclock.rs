//! kvmclock, which lets the guest compute the time from its TSC without exits. It is
//! configured per vCPU through MSRs.

use std::time::{SystemTime, UNIX_EPOCH};

use midgard_hv::{GuestMemory, MemoryError};
use zerocopy::{Immutable, IntoBytes};

use crate::clock::{host_ns, rdtsc};

/// Bit 0 of the system time MSR enables kvmclock.
const SYSTEM_TIME_ENABLE: u64 = 1;

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

/// Per-vCPU kvmclock state.
pub(crate) struct Kvmclock {
    /// Converts TSC ticks to nanoseconds (see [`time_scale`]).
    tsc_shift: i8,
    tsc_to_system_mul: u32,
    msr: u64,
    version: u32,
    /// Host time of the last update.
    published_at: Option<u64>,
}

impl Kvmclock {
    pub(crate) fn new(tsc_hz: u64) -> Self {
        let (tsc_shift, tsc_to_system_mul) = time_scale(1_000_000_000, tsc_hz);
        Self {
            tsc_shift,
            tsc_to_system_mul,
            msr: 0,
            version: 0,
            published_at: None,
        }
    }

    pub(crate) fn msr(&self) -> u64 {
        self.msr
    }

    pub(crate) fn set_msr(&mut self, memory: &GuestMemory, msr: u64) -> Result<(), MemoryError> {
        self.msr = msr;
        self.published_at = None;
        self.update(memory)
    }

    /// Re-publishes the time structure if it is outdated, such that errors in the
    /// TSC frequency do not accumulate. Must only be called while the vCPU does not run.
    pub(crate) fn update(&mut self, memory: &GuestMemory) -> Result<(), MemoryError> {
        const REFRESH_NS: u64 = 1_000_000_000;
        if self.msr & SYSTEM_TIME_ENABLE == 0 {
            return Ok(());
        }
        let now = host_ns();
        if self.published_at.is_some_and(|at| now - at < REFRESH_NS) {
            return Ok(());
        }
        let (ns, tsc) = (host_ns(), rdtsc());
        // The guest does not run while we update the structure, so the version
        // does not need to be odd during the update.
        self.version = self.version.wrapping_add(2);
        let info = TimeInfo {
            version: self.version,
            pad0: 0,
            tsc_timestamp: tsc,
            system_time: ns,
            tsc_to_system_mul: self.tsc_to_system_mul,
            tsc_shift: self.tsc_shift,
            // Without PVCLOCK_TSC_STABLE_BIT, Linux keeps the clock monotonic itself
            // (our updates may make it jump backwards slightly).
            flags: 0,
            pad: [0; 2],
        };
        self.published_at = Some(now);
        memory.write_obj(self.msr & !SYSTEM_TIME_ENABLE, &info)
    }
}

/// Writes struct pvclock_wall_clock: the wall clock time at which the system time was zero.
pub(crate) fn write_wall_clock(memory: &GuestMemory, address: u64) -> Result<(), MemoryError> {
    let wall_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let boot = wall_ns.saturating_sub(host_ns());
    let wall = WallClock {
        version: 2,
        sec: (boot / 1_000_000_000) as u32,
        nsec: (boot % 1_000_000_000) as u32,
    };
    memory.write_obj(address, &wall)
}
