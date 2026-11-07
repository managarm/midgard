//! Host time: the TSC (the guest's TSC is the host's TSC) and the host's monotonic
//! clock.

pub(crate) fn rdtsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    #[allow(unused_unsafe)]
    // SAFETY: RDTSC has no side effects.
    return unsafe { core::arch::x86_64::_rdtsc() };
    #[cfg(not(target_arch = "x86_64"))]
    return 0;
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

/// Other kernels do not tell us how they convert the TSC; we measure it.
#[cfg(not(target_os = "managarm"))]
fn kernel_tsc_hz() -> Option<u64> {
    None
}

/// Determines the TSC frequency: exactly from the kernel's clock if possible
/// (then kvmclock agrees with helGetClock()), otherwise by measuring it.
pub(crate) fn calibrate() -> u64 {
    if let Some(hz) = kernel_tsc_hz() {
        return hz;
    }
    const DURATION_NS: u64 = 200_000_000;
    let (start_ns, start_tsc) = (host_ns(), rdtsc());
    let (end_ns, end_tsc) = loop {
        let (ns, tsc) = (host_ns(), rdtsc());
        if ns - start_ns >= DURATION_NS {
            break (ns, tsc);
        }
    };
    ((end_tsc - start_tsc) as u128 * 1_000_000_000 / (end_ns - start_ns) as u128) as u64
}
