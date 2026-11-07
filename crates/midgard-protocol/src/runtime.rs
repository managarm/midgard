//! Where midgard keeps its runtime files (the socket that bifrost connects to), which
//! bifrost and midgard both need to agree on.

use std::path::PathBuf;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The directory with midgard's runtime files: $MIDGARD_RUNTIME_DIR,
/// $XDG_RUNTIME_DIR/midgard or /run/midgard.
pub fn runtime_root() -> PathBuf {
    env_path("MIDGARD_RUNTIME_DIR")
        .or_else(|| env_path("XDG_RUNTIME_DIR").map(|dir| dir.join("midgard")))
        .unwrap_or_else(|| "/run/midgard".into())
}

/// The socket that midgard listens on (systemd's midgard.socket creates it).
pub fn daemon_socket() -> PathBuf {
    runtime_root().join("midgard.sock")
}
