//! Boot files, which midgard boots guests with: a directory in midgard's boot directory
//! (see [`paths::boot_dir`]; guest/build.py installs them) with the guest kernel
//! (`vmlinuz`), the agent (`midgard-init`), the guest's waypipe (`guest-waypipe`, if
//! there is one) and `manifest.json`, which says what the files are. They are built and
//! distributed apart from midgard (and from the templates, whose instances say which
//! boot files they boot with), so the manifest says which versions of the guest protocol
//! the agent speaks.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use midgard_protocol::version::{Mismatch, Versions};
use serde::{Deserialize, Serialize};

use crate::instance::validate_name;
use crate::paths;

/// The boot files of VM instances that do not name theirs.
pub const DEFAULT: &str = "linux";

/// The guest kernel in the boot files' directory.
const KERNEL: &str = "vmlinuz";
/// The agent in the boot files' directory.
const AGENT: &str = "midgard-init";
/// The files that boot files need to have.
pub const REQUIRED: [&str; 2] = [KERNEL, AGENT];
/// The guest's waypipe (a static Linux build) in the boot files' directory.
const WAYPIPE: &str = "guest-waypipe";
/// The manifest in the boot files' directory.
pub const MANIFEST: &str = "manifest.json";

/// What `manifest.json` says about boot files. (Files that were put there by hand may
/// lack it.)
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The versions of the guest protocol that the agent speaks.
    pub protocol: Option<Versions>,
    /// The SHA-256 of the files, by name.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

/// The boot files of a guest.
#[derive(Debug)]
pub struct BootFiles {
    pub kernel: PathBuf,
    pub agent: PathBuf,
    pub waypipe: Option<PathBuf>,
    pub manifest: Manifest,
}

impl BootFiles {
    /// Finds the boot files `name`.
    pub fn load(name: &str) -> Result<Self> {
        validate_name(name)?;
        let dir = paths::boot_dir(name);
        let missing = REQUIRED
            .iter()
            .filter(|file| !dir.join(file).is_file())
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!(
                "the boot files {name} lack {} (in {}; guest/build.py install installs them)",
                missing.join(" and "),
                dir.display()
            );
        }
        let manifest = read_manifest(&dir)?;
        Ok(Self {
            kernel: dir.join(KERNEL),
            agent: dir.join(AGENT),
            waypipe: Some(dir.join(WAYPIPE)).filter(|path| path.is_file()),
            manifest,
        })
    }

    /// Fails if the agent speaks no version of the guest protocol that we speak, as far
    /// as the manifest tells (the agent itself says it once it runs).
    pub fn check(&self) -> Result<()> {
        self.manifest.protocol.map(negotiate).transpose()?;
        Ok(())
    }
}

/// Reads the manifest of the boot files in `dir` (the default one if there is none).
pub fn read_manifest(dir: &Path) -> Result<Manifest> {
    let path = dir.join(MANIFEST);
    match std::fs::read(&path) {
        Ok(text) => serde_json::from_slice(&text).with_context(|| format!("invalid {path:?}")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Manifest::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {path:?}")),
    }
}

/// Writes the manifest of the boot files in `dir` (which is new) and syncs it.
pub fn write_manifest(dir: &Path, manifest: &Manifest) -> Result<()> {
    let path = dir.join(MANIFEST);
    let mut text = serde_json::to_vec_pretty(manifest)?;
    text.push(b'\n');
    File::create_new(&path)
        .and_then(|mut file| {
            file.write_all(&text)?;
            file.sync_all()
        })
        .with_context(|| format!("failed to write {path:?}"))
}

/// The version of the guest protocol to speak with an agent that speaks `agent`.
pub fn negotiate(agent: Versions) -> Result<u32> {
    Versions::OURS.negotiate(agent).map_err(|mismatch| {
        let advice = match mismatch {
            Mismatch::PeerTooOld { .. } => "is too old for this midgard: update the boot files",
            Mismatch::PeerTooNew { .. } => "is too new for this midgard: update midgard",
        };
        anyhow!("the guest agent (midgard-init) {advice} (it speaks {mismatch})")
    })
}

#[cfg(test)]
mod tests {
    use midgard_protocol::version::{OLDEST_PROTOCOL, PROTOCOL};

    use super::*;

    #[test]
    fn manifests() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"protocol": {"oldest": 1, "current": 3}, "files": {"vmlinuz": "abc"}, "new": []}"#,
        )
        .unwrap();
        assert_eq!(
            manifest.protocol,
            Some(Versions {
                oldest: 1,
                current: 3
            })
        );
        assert_eq!(manifest.files["vmlinuz"], "abc");
        assert_eq!(
            serde_json::from_str::<Manifest>("{}").unwrap(),
            Manifest::default()
        );
    }

    #[test]
    fn agents() {
        assert_eq!(negotiate(Versions::OURS).unwrap(), PROTOCOL);
        let newer = Versions {
            oldest: PROTOCOL + 1,
            current: PROTOCOL + 1,
        };
        let error = negotiate(newer).unwrap_err().to_string();
        assert!(error.contains("update midgard"), "{error}");
        if OLDEST_PROTOCOL > 1 {
            let older = Versions {
                oldest: 1,
                current: 1,
            };
            let error = negotiate(older).unwrap_err().to_string();
            assert!(error.contains("update the boot files"), "{error}");
        }
    }
}
