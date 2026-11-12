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
use midgard_protocol::size::format_size;
use midgard_vmm::image;

use crate::settings::{self, Role, Settings, Share};
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
/// The size of the guest's disk sectors.
const SECTOR_SIZE: u64 = 512;

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
        (self.settings.disk.clone()).unwrap_or_else(|| self.dir.join(DISK))
    }

    /// The instance's VM.
    pub fn config(&self) -> MachineConfig {
        MachineConfig {
            kernel: (self.settings.kernel.clone()).unwrap_or_else(paths::kernel),
            initrd: None,
            agent: Some(paths::agent()),
            cmdline: None,
            append: Vec::new(),
            memory: self.settings.memory.unwrap_or(DEFAULT_MEMORY),
            cpus: self.settings.cpus,
            console: ConsoleKind::Virtio,
            disks: vec![Disk {
                path: self.disk(),
                read_only: false,
            }],
            network: self.settings.network.unwrap_or(true),
            share: match &self.settings.share {
                None => Some("/".into()),
                Some(Share::Nothing) => None,
                Some(Share::Directory(dir)) => Some(dir.clone()),
            },
            guest: self.settings.guest.clone(),
            waypipe: if self.settings.gui.unwrap_or(true) {
                paths::guest_waypipe()
            } else {
                None
            },
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

/// Creates the instance `name` from `template`, with a disk of `disk_size` bytes if it is
/// given (which cannot be smaller than the template's; midgard-init grows the file
/// system to fit): its directory is made next to where it belongs and moved there once
/// it is complete. Nothing else may create or delete the instance meanwhile (see
/// [`crate::daemon::Instances::reserve`]).
pub fn create(
    name: &str,
    template: &str,
    user: Option<&str>,
    memory: Option<u64>,
    disk_size: Option<u64>,
) -> Result<()> {
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
        let disk = partial.join(DISK);
        source.copy_disk(&disk)?;
        if let Some(size) = disk_size {
            grow_disk(&disk, template, size)?;
        }
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

/// Grows the disk that was copied from `template` to `size` bytes.
fn grow_disk(disk: &Path, template: &str, size: u64) -> Result<()> {
    // The guest sees whole sectors.
    if !size.is_multiple_of(SECTOR_SIZE) {
        bail!("the disk's size must be a multiple of {SECTOR_SIZE} bytes");
    }
    let current = image::size(disk).with_context(|| format!("failed to read {disk:?}"))?;
    if size < current {
        bail!(
            "the disk of the template {template} has {}; the disk cannot be smaller",
            format_size(current)
        );
    }
    image::grow(disk, size).with_context(|| format!("failed to grow {disk:?}"))
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
    fn grow_disks() {
        let dir = std::env::temp_dir().join(format!("midgard-grow-disk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A raw image (images are detected by their magic, not by their name).
        let disk = dir.join(DISK);
        File::create(&disk).unwrap().set_len(1 << 20).unwrap();
        assert!(grow_disk(&disk, "alpine", 4096).is_err());
        assert!(grow_disk(&disk, "alpine", (2 << 20) + 1).is_err());
        grow_disk(&disk, "alpine", 2 << 20).unwrap();
        assert_eq!(image::size(&disk).unwrap(), 2 << 20);
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
