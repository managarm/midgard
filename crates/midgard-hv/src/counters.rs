//! The exit counts that the backends keep for each vCPU (see [`crate::Counters`]).

use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Exit;

/// The kinds of exits that a group of counters counts: each kind has a counter and a
/// name (see `kinds!`).
pub(crate) trait Kind: Copy {
    /// The kinds' names, by [`Kind::index`].
    const NAMES: &'static [&'static str];

    fn index(self) -> usize;
}

/// Defines an enum that is a [`Kind`], from its variants and their names.
macro_rules! kinds {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $text:literal,)*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy)]
        $vis enum $name {
            $($(#[$variant_meta])* $variant,)*
        }

        impl $crate::counters::Kind for $name {
            const NAMES: &'static [&'static str] = &[$($text,)*];

            fn index(self) -> usize {
                self as usize
            }
        }
    };
}
pub(crate) use kinds;

kinds! {
    /// The exits that every backend returns (see [`ExitCounts::count`]).
    enum Returned {
        Io => "exit.io",
        Mmio => "exit.mmio",
        Rdmsr => "exit.rdmsr",
        Wrmsr => "exit.wrmsr",
        Kicked => "exit.kicked",
        Hypercall => "exit.hypercall",
    }
}

/// Counts a vCPU's exits: those that the backend returns, and those that it handles
/// itself (the kinds `I`, which the backend defines). The vCPU's thread counts; any
/// thread reads.
pub(crate) struct ExitCounts<I> {
    returned: [AtomicU64; Returned::NAMES.len()],
    internal: Box<[AtomicU64]>,
    kinds: PhantomData<I>,
}

impl<I: Kind> ExitCounts<I> {
    pub(crate) fn new() -> Self {
        Self {
            returned: Default::default(),
            internal: I::NAMES.iter().map(|_| AtomicU64::new(0)).collect(),
            kinds: PhantomData,
        }
    }

    /// Counts an exit that the backend returns.
    pub(crate) fn count(&self, exit: &Exit) {
        let kind = match exit {
            Exit::Io { .. } => Returned::Io,
            Exit::Mmio { .. } => Returned::Mmio,
            Exit::Rdmsr { .. } => Returned::Rdmsr,
            Exit::Wrmsr { .. } => Returned::Wrmsr,
            Exit::Kicked => Returned::Kicked,
            Exit::Hypercall => Returned::Hypercall,
            // The VM stops on these, so nothing would read their counts.
            Exit::Shutdown | Exit::Failed(_) => return,
        };
        self.returned[kind.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Counts an exit that the backend handled itself.
    pub(crate) fn count_internal(&self, kind: I) {
        self.internal[kind.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Appends the counts to `out`.
    pub(crate) fn snapshot(&self, out: &mut Vec<(String, u64)>) {
        let names = Returned::NAMES.iter().chain(I::NAMES);
        let values = self.returned.iter().chain(self.internal.iter());
        out.extend(
            names
                .zip(values)
                .map(|(name, value)| (name.to_string(), value.load(Ordering::Relaxed))),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    kinds! {
        enum Internal {
            Cpuid => "exit.cpuid",
            Eoi => "exit.eoi",
        }
    }

    #[test]
    fn counts_exits_by_kind() {
        let counts = ExitCounts::<Internal>::new();
        counts.count(&Exit::Kicked);
        counts.count(&Exit::Kicked);
        counts.count(&Exit::Hypercall);
        counts.count(&Exit::Failed("x".into()));
        counts.count_internal(Internal::Cpuid);
        counts.count_internal(Internal::Eoi);
        counts.count_internal(Internal::Eoi);
        let mut out = Vec::new();
        counts.snapshot(&mut out);
        let get = |name: &str| out.iter().find(|(n, _)| n == name).map(|&(_, v)| v);
        assert_eq!(get("exit.kicked"), Some(2));
        assert_eq!(get("exit.hypercall"), Some(1));
        assert_eq!(get("exit.io"), Some(0));
        assert_eq!(get("exit.cpuid"), Some(1));
        assert_eq!(get("exit.eoi"), Some(2));
        assert_eq!(get("exit.failed"), None);
        assert_eq!(out.len(), Returned::NAMES.len() + Internal::NAMES.len());
    }
}
