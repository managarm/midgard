//! midgard's messages go to stderr (the daemon's is the console log), prefixed with
//! "midgard: ".

use std::collections::BTreeSet;
use std::sync::Mutex;

/// Prints a message (like `eprintln!`) with midgard's prefix.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { eprintln!("midgard: {}", format_args!($($arg)*)) };
}

/// Remembers which problems were reported, such that a guest that keeps doing something
/// unsupported does not flood the log.
pub(crate) struct ReportOnce<K>(Mutex<BTreeSet<K>>);

impl<K: Ord> ReportOnce<K> {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(BTreeSet::new()))
    }

    /// Returns true the first time that `key` is passed (the problem should be reported).
    pub(crate) fn first(&self, key: K) -> bool {
        self.0.lock().unwrap().insert(key)
    }
}
