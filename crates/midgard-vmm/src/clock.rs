//! Host time: the TSC (the guest's TSC is the host's TSC), the host's monotonic clock
//! and its wall clock.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) fn rdtsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    #[allow(unused_unsafe)]
    // SAFETY: RDTSC has no side effects.
    return unsafe { core::arch::x86_64::_rdtsc() };
    #[cfg(not(target_arch = "x86_64"))]
    return 0;
}

/// Nanoseconds since the host booted (helGetClock()).
#[cfg(target_os = "managarm")]
pub(crate) fn host_ns() -> u64 {
    hel::Time::new_since_boot()
        .expect("helGetClock cannot fail")
        .nanos()
}

/// Nanoseconds since the host booted (CLOCK_MONOTONIC).
#[cfg(not(target_os = "managarm"))]
pub(crate) fn host_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime() only writes the timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

/// The host's wall clock: the time since the Unix epoch (zero before it).
pub(crate) fn unix_time() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}
