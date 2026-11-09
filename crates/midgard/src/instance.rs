//! VM instances: a directory in midgard's instances directory (see
//! [`paths::instances_dir`]) with the instance's settings (`instance.toml`, see
//! [`crate::settings`]) and its disk (`disk.qcow2`), which midgard boots with its guest
//! kernel and the agent.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use midgard_protocol::control::InstanceInfo;
use midgard_protocol::machine::{ConsoleKind, Disk, MachineConfig};
use midgard_protocol::runtime;

use crate::settings::{self, Role, Settings};
use crate::template::Template;
use crate::{log, paths};

/// The longest name of an instance.
const MAX_NAME: usize = 64;
/// The guest's RAM unless the instance's settings say otherwise.
const DEFAULT_MEMORY: u64 = 1 << 30;

/// The instance file in an instance's directory.
const INSTANCE_FILE: &str = "instance.toml";
/// The disk in an instance's directory.
const DISK: &str = "disk.qcow2";

/// Checks the name of an instance: it names a directory, so it consists of letters,
/// digits, `-` and `_` and does not start with `-`.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        bail!(
            "invalid name {name:?} (use up to {MAX_NAME} letters, digits, '-' and '_', \
             not starting with '-')"
        );
    }
    Ok(())
}

/// A VM instance.
pub struct Instance {
    /// The instance's directory.
    dir: PathBuf,
    pub settings: Settings,
}

impl Instance {
    /// Reads the instance `name`.
    pub fn load(name: &str) -> Result<Self> {
        validate_name(name)?;
        let dir = paths::instances_dir().join(name);
        let path = dir.join(INSTANCE_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound && !dir.is_dir() => {
                bail!(
                    "no VM instance {name} (in {})",
                    paths::instances_dir().display()
                )
            }
            Err(error) => return Err(error).with_context(|| format!("failed to read {path:?}")),
        };
        let settings =
            settings::parse(&text, Role::Instance).with_context(|| format!("invalid {path:?}"))?;
        Ok(Self { dir, settings })
    }

    /// The instance's disk.
    pub fn disk(&self) -> PathBuf {
        self.dir.join(DISK)
    }

    /// The instance's VM.
    pub fn config(&self) -> MachineConfig {
        MachineConfig {
            kernel: paths::kernel(),
            initrd: None,
            agent: Some(paths::agent()),
            cmdline: None,
            append: Vec::new(),
            memory: self.settings.memory.unwrap_or(DEFAULT_MEMORY),
            cpus: None,
            console: ConsoleKind::Virtio,
            disks: vec![Disk {
                path: self.disk(),
                read_only: false,
            }],
            network: true,
            share: Some("/".into()),
            guest: self.settings.guest.clone(),
            waypipe: paths::guest_waypipe(),
        }
    }
}

/// Writes a new file and syncs it.
fn write_synced(path: &Path, text: &str) -> io::Result<()> {
    let mut file = File::create_new(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// Syncs a directory, such that the entries in it are durable (best effort: not all file
/// systems can sync directories).
fn sync_dir(path: &Path) {
    if let Ok(dir) = File::open(path) {
        let _ = dir.sync_all();
    }
}

/// Removes what creating or deleting instances left behind when midgard stopped in the
/// middle (see [`create`] and [`delete`]). Nothing may create or delete instances
/// meanwhile.
pub fn remove_leftovers() {
    let instances = paths::instances_dir();
    let Ok(entries) = std::fs::read_dir(&instances) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let leftover = [".partial", ".deleted"]
            .iter()
            .any(|suffix| name.ends_with(suffix));
        if name.starts_with('.') && leftover {
            log!("removing {name}, which creating or deleting an instance left behind");
            if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                log!("failed to remove {:?}: {error}", entry.path());
            }
        }
    }
}

/// Whether the instance `name` exists (which it does even if it is broken).
pub fn exists(name: &str) -> bool {
    paths::instances_dir().join(name).is_dir()
}

/// Creates the instance `name` from `template`: its directory is made next to where it
/// belongs and moved there once it is complete. Nothing else may create or delete the
/// instance meanwhile (see [`crate::daemon::Instances::reserve`]).
pub fn create(name: &str, template: &str, user: Option<&str>, memory: Option<u64>) -> Result<()> {
    validate_name(name)?;
    let source = Template::load(template)?;
    let instances = paths::instances_dir();
    let dir = instances.join(name);
    if dir.exists() {
        bail!("the VM instance {name} exists already");
    }
    let text = settings::instance_file(&source.text, template, memory, user)
        .with_context(|| format!("cannot create {name} from the template {template}"))?;
    // (Names do not contain dots, so this is no instance.)
    let partial = instances.join(format!(".{name}.partial"));
    if partial.exists() {
        std::fs::remove_dir_all(&partial)
            .with_context(|| format!("failed to delete {partial:?}"))?;
    }
    std::fs::create_dir_all(&partial).with_context(|| format!("failed to create {partial:?}"))?;
    let result = (|| {
        log!("creating {name} from the template {template}");
        source.copy_disk(&partial.join(DISK))?;
        let file = partial.join(INSTANCE_FILE);
        write_synced(&file, &text).with_context(|| format!("failed to write {file:?}"))?;
        sync_dir(&partial);
        std::fs::rename(&partial, &dir).with_context(|| format!("failed to create {dir:?}"))?;
        sync_dir(&instances);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&partial);
    }
    result
}

/// Deletes the instance `name`: its directory is moved out of the way first, such that a
/// failure does not leave half an instance behind. Its VM must not run, and nothing else
/// may create or delete the instance meanwhile (see
/// [`crate::daemon::Instances::reserve`]).
pub fn delete(name: &str) -> Result<()> {
    validate_name(name)?;
    let instances = paths::instances_dir();
    let dir = instances.join(name);
    if !dir.is_dir() {
        bail!("no VM instance {name} (in {})", instances.display());
    }
    // (Names do not contain dots, so this is no instance.)
    let deleted = instances.join(format!(".{name}.deleted"));
    if deleted.exists() {
        std::fs::remove_dir_all(&deleted)
            .with_context(|| format!("failed to delete {deleted:?}"))?;
    }
    std::fs::rename(&dir, &deleted).with_context(|| format!("failed to delete {dir:?}"))?;
    log!("deleting {name}");
    std::fs::remove_dir_all(&deleted).with_context(|| format!("failed to delete {deleted:?}"))?;
    // Its console log.
    let runtime = runtime::runtime_dir(name);
    match std::fs::remove_dir_all(&runtime) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("failed to delete {runtime:?}"))
        }
        _ => Ok(()),
    }
}

/// The instance `name` for `bifrost list` (`running`: whether its VM runs).
pub fn info(name: &str, running: bool) -> InstanceInfo {
    let mut info = InstanceInfo {
        name: name.to_string(),
        template: None,
        running,
        disk_size: None,
        error: None,
    };
    match Instance::load(name) {
        Ok(instance) => {
            info.disk_size = std::fs::metadata(instance.disk()).ok().map(|m| m.len());
            info.template = instance.settings.template;
        }
        Err(error) => info.error = Some(format!("{error:#}")),
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("alpine").is_ok());
        assert!(validate_name("dev_2-x").is_ok());
        for name in [
            "",
            "-x",
            "../etc",
            "a/b",
            "a b",
            "a.b",
            &"x".repeat(MAX_NAME + 1),
        ] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
    }
}
