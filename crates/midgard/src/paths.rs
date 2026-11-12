//! Where midgard's files are: the boot files, the templates and the VM instances. The
//! daemon's runtime files are in [`midgard_protocol::runtime`].

use std::path::{Path, PathBuf};

/// Where packages install boot files.
const PACKAGE_DIR: &str = "/usr/lib/midgard";

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// `$HOME/<relative>`.
fn in_home(relative: &str) -> Option<PathBuf> {
    env_path("HOME").map(|home| home.join(relative))
}

/// midgard's directory: MIDGARD_DATA_DIR, else /var/lib/midgard for root and
/// $XDG_DATA_HOME/midgard (~/.local/share/midgard) for other users. It has the VM
/// instances and the boot files and templates that the user installed (see
/// guest/build.py).
pub fn midgard_dir() -> PathBuf {
    if let Some(dir) = env_path("MIDGARD_DATA_DIR") {
        return dir;
    }
    // SAFETY: getuid() has no preconditions.
    if unsafe { libc::getuid() } == 0 {
        return "/var/lib/midgard".into();
    }
    env_path("XDG_DATA_HOME")
        .or_else(|| in_home(".local/share"))
        .unwrap_or_else(|| "/var/lib".into())
        .join("midgard")
}

/// The directory of the boot files `name` (see [`crate::boot`]): the one in
/// [`midgard_dir`] if it exists, else the one that packages install (such that errors
/// name it).
pub fn boot_dir(name: &str) -> PathBuf {
    let path = midgard_dir().join("boot").join(name);
    if path.is_dir() {
        return path;
    }
    PathBuf::from(PACKAGE_DIR).join("boot").join(name)
}

/// The VM instances' directories (see [`crate::instance`]).
pub fn instances_dir() -> PathBuf {
    midgard_dir().join("instances")
}

/// The templates' directories (see [`crate::template`]).
pub fn templates_dir() -> PathBuf {
    midgard_dir().join("templates")
}

/// The names of the instances or templates in `dir` (sorted): its entries whose names
/// are valid (see [`crate::instance::validate_name`]), which leaves out others, e.g.,
/// those that are being created.
pub fn names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| crate::instance::validate_name(name).is_ok())
        .collect();
    names.sort();
    names
}
