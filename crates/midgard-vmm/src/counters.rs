//! Counters of what the guest does with the devices, for diagnostics (see
//! [`crate::MachineHandle::counters`]).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// A counter that any thread increments.
#[derive(Default)]
pub(crate) struct Counter(AtomicU64);

impl Counter {
    pub(crate) fn add(&self) {
        // Nothing synchronizes with a counter.
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// The reads and writes of a device's registers (`what` names their kind in
/// [`Accesses::snapshot`], e.g., `pio`).
#[derive(Default)]
pub(crate) struct Accesses {
    pub(crate) reads: Counter,
    pub(crate) writes: Counter,
}

impl Accesses {
    /// Adds `<device>.<what>_reads` and `<device>.<what>_writes` to `out`.
    pub(crate) fn snapshot(&self, device: &str, what: &str, out: &mut BTreeMap<String, u64>) {
        out.insert(format!("{device}.{what}_reads"), self.reads.get());
        out.insert(format!("{device}.{what}_writes"), self.writes.get());
    }
}
