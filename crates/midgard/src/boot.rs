//! Boot files, which midgard boots guests with: a directory in midgard's boot directory
//! (see [`paths::boot_dir`]; `bifrost pull` installs them, see [`crate::pull`]) with the
//! guest kernel and what else the guest boots with, `boot.toml`, which says what the
//! files are and how the guest boots with them (see [`Description`]), and
//! `manifest.json`, which says what was installed (see [`Manifest`]).
//!
//! They are built and distributed apart from midgard (and from the templates, whose
//! instances say which boot files they boot with), so they describe themselves, such that
//! midgard boots guests that it does not know:
//!
//! ```toml
//! # The format of the file (default: 1), see [`FORMAT`].
//! format = 1
//!
//! # The guest kernel: a bzImage, or an ELF image that boots through multiboot2.
//! kernel = "vmlinuz"
//! # The initrd (default: none; not with agent = "initramfs").
//! initrd = "initrd.cpio"
//! # Where midgard's agent comes from: "initramfs" (default: midgard boots the kernel
//! # with an initramfs that runs `midgard-init` and `midgard-agent` of the boot files,
//! # which start the distribution's init) or "image" (the guest's image has the agent,
//! # which the guest starts if `midgard.agent` is on the kernel command line, see
//! # [`midgard_protocol::param`]; its settings name no init, see [`crate::settings`]).
//! agent = "initramfs"
//! # The guest's waypipe (default: none; only with agent = "initramfs"), with which
//! # midgard forwards the guest's Wayland clients if the boot files have it.
//! waypipe = "guest-waypipe"
//! # The guest's console: "virtio" (default: virtio-console) or "serial" (the serial
//! # port).
//! console = "virtio"
//! # Whether the guest sees the host's files (virtio-fs; default: true).
//! share = true
//! # The kernel's command line (default: the one that midgard builds for Linux with
//! # midgard's agent). midgard replaces `{guest-ip}`, `{netmask}`, `{gateway}` and `{dns}`
//! # with the addresses of the guest's network and adds its own parameters.
//! cmdline = "console=hvc0 ip={guest-ip}::{gateway}:{netmask}:midgard:eth0:off:{dns}"
//! ```
//!
//! Boot files without `boot.toml` are those of Linux from before boot files had one (see
//! [`Description::default`]).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use midgard_protocol::guest::{AGENT_FILE, INIT_FILE};
use midgard_protocol::machine::ConsoleKind;
use midgard_protocol::version::{Mismatch, Versions};
use midgard_vmm::network;
use serde::{Deserialize, Serialize};

use crate::instance::validate_name;
use crate::{paths, settings};

/// The boot files of VM instances that do not name theirs.
pub const DEFAULT: &str = "linux";

/// The description in the boot files' directory.
pub const DESCRIPTION: &str = "boot.toml";
/// The newest format of [`DESCRIPTION`] that this midgard reads. Boot files are
/// distributed apart from midgard, so a description that has what an older format lacks
/// says so (`format`), such that an older midgard can tell that it is too old.
pub const FORMAT: u32 = 1;
/// The manifest in the boot files' directory.
pub const MANIFEST: &str = "manifest.json";

/// The guest kernel of boot files without a description.
const KERNEL: &str = "vmlinuz";
/// The guest's waypipe (a static Linux build) of boot files without a description.
const WAYPIPE: &str = "guest-waypipe";

/// Where midgard's agent in the guest comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    /// The initramfs that midgard boots the kernel with, which runs the agent of the
    /// boot files ([`INIT_FILE`] and [`AGENT_FILE`]).
    #[default]
    Initramfs,
    /// The guest's image, and the guest starts it (see
    /// [`midgard_protocol::machine::MachineConfig::agent_in_image`]).
    Image,
}

/// What boot files are and how the guest boots with them ([`DESCRIPTION`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    /// The guest kernel.
    pub kernel: String,
    pub initrd: Option<String>,
    pub agent: Agent,
    /// The guest's waypipe, which the boot files may lack.
    pub waypipe: Option<String>,
    pub console: ConsoleKind,
    /// Whether the guest sees the host's files.
    pub share: bool,
    /// The kernel's command line, with the addresses of the guest's network (None: the
    /// one that midgard builds for Linux, see
    /// [`midgard_protocol::machine::MachineConfig::cmdline`]).
    pub cmdline: Option<String>,
}

/// The contents of [`DESCRIPTION`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct DescriptionFile {
    #[expect(dead_code, reason = "checked before the rest of the file is read")]
    format: Option<u32>,
    kernel: String,
    initrd: Option<String>,
    #[serde(default)]
    agent: Agent,
    waypipe: Option<String>,
    console: Option<ConsoleSetting>,
    share: Option<bool>,
    cmdline: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ConsoleSetting {
    Virtio,
    Serial,
}

/// The boot files of Linux from before boot files had a description.
impl Default for Description {
    fn default() -> Self {
        Self {
            kernel: KERNEL.into(),
            initrd: None,
            agent: Agent::Initramfs,
            waypipe: Some(WAYPIPE.into()),
            console: ConsoleKind::Virtio,
            share: true,
            cmdline: None,
        }
    }
}

impl Description {
    /// Parses a description.
    pub fn parse(text: &str) -> Result<Self> {
        settings::check_format(text, FORMAT)?;
        let file: DescriptionFile = toml::from_str(text)?;
        let files = [
            ("kernel", Some(&file.kernel)),
            ("initrd", file.initrd.as_ref()),
            ("waypipe", file.waypipe.as_ref()),
        ];
        for (key, name) in files {
            if let Some(name) = name
                && !is_file_name(name)
            {
                bail!("{key} must be the name of a file of the boot files (not {name:?})");
            }
        }
        match file.agent {
            Agent::Initramfs if file.initrd.is_some() => {
                bail!("initrd needs agent = \"image\" (midgard builds the agent's initramfs)")
            }
            Agent::Image if file.waypipe.is_some() => {
                bail!("waypipe needs agent = \"initramfs\" (the guest has its own waypipe)")
            }
            _ => {}
        }
        let cmdline = file
            .cmdline
            .as_deref()
            .map(expand)
            .transpose()
            .context("invalid cmdline")?;
        Ok(Self {
            kernel: file.kernel,
            initrd: file.initrd,
            agent: file.agent,
            waypipe: file.waypipe,
            console: match file.console {
                None | Some(ConsoleSetting::Virtio) => ConsoleKind::Virtio,
                Some(ConsoleSetting::Serial) => ConsoleKind::Serial,
            },
            share: file.share.unwrap_or(true),
            cmdline,
        })
    }

    /// The files that the boot files need to have.
    pub fn required(&self) -> Vec<&str> {
        let mut files = vec![self.kernel.as_str()];
        files.extend(self.initrd.as_deref());
        if self.agent == Agent::Initramfs {
            files.extend([INIT_FILE, AGENT_FILE]);
        }
        files
    }
}

/// Whether `name` can be a file of boot files: a plain name that is none of our own (those
/// start with a dot, or are the manifest).
pub fn is_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name != MANIFEST
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Replaces the placeholders of a kernel command line with the addresses of the guest's
/// network.
fn expand(cmdline: &str) -> Result<String> {
    if cmdline.chars().any(char::is_control) {
        bail!("it must be a single line");
    }
    let mut expanded = String::new();
    let mut rest = cmdline;
    while let Some(start) = rest.find('{') {
        let Some(length) = rest[start..].find('}') else {
            break;
        };
        let address = match &rest[start + 1..start + length] {
            "guest-ip" => network::GUEST_IP,
            "netmask" => network::NETMASK,
            "gateway" => network::HOST_IP,
            "dns" => network::DNS_IP,
            name => bail!(
                "unknown placeholder {{{name}}} (there are {{guest-ip}}, {{netmask}}, \
                 {{gateway}} and {{dns}})"
            ),
        };
        expanded.push_str(&rest[..start]);
        expanded.push_str(&address.to_string());
        rest = &rest[start + length + 1..];
    }
    expanded.push_str(rest);
    Ok(expanded)
}

/// Reads the description of the boot files in `dir` (the one of Linux if there is none).
pub fn read_description(dir: &Path) -> Result<Description> {
    let path = dir.join(DESCRIPTION);
    match std::fs::read_to_string(&path) {
        Ok(text) => Description::parse(&text).with_context(|| format!("invalid {path:?}")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Description::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {path:?}")),
    }
}

/// What `manifest.json` says about boot files: what `bifrost pull` installed. (Files that
/// were put there by hand may lack it.)
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
    /// The boot files' directory.
    dir: PathBuf,
    pub description: Description,
    pub manifest: Manifest,
}

impl BootFiles {
    /// Finds the boot files `name`.
    pub fn load(name: &str) -> Result<Self> {
        validate_name(name)?;
        Self::load_from(name, paths::boot_dir(name))
    }

    /// Finds the boot files `name` in their directory `dir`.
    fn load_from(name: &str, dir: PathBuf) -> Result<Self> {
        let description = read_description(&dir)?;
        let missing = description
            .required()
            .into_iter()
            .filter(|file| !dir.join(file).is_file())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            // (Boot files of an older version of the guest protocol lack files that we
            // need; then the version says more.)
            if let Ok(Manifest {
                protocol: Some(protocol),
                ..
            }) = read_manifest(&dir)
            {
                negotiate(protocol)?;
            }
            bail!(
                "the boot files {name} lack {} (in {}; bifrost pull installs them)",
                missing.join(" and "),
                dir.display()
            );
        }
        let manifest = read_manifest(&dir)?;
        Ok(Self {
            dir,
            description,
            manifest,
        })
    }

    /// Fails if the agent speaks no version of the guest protocol that we speak, as far
    /// as the manifest tells (the agent itself says it once it runs).
    pub fn check(&self) -> Result<()> {
        self.manifest.protocol.map(negotiate).transpose()?;
        Ok(())
    }

    pub fn kernel(&self) -> PathBuf {
        self.dir.join(&self.description.kernel)
    }

    pub fn initrd(&self) -> Option<PathBuf> {
        Some(self.dir.join(self.description.initrd.as_ref()?))
    }

    /// The directory with the agent's files (the boot files' directory) if midgard starts
    /// the agent.
    pub fn agent(&self) -> Option<PathBuf> {
        (self.description.agent == Agent::Initramfs).then(|| self.dir.clone())
    }

    /// The guest's waypipe, if the boot files have one.
    pub fn waypipe(&self) -> Option<PathBuf> {
        Some(self.dir.join(self.description.waypipe.as_ref()?)).filter(|path| path.is_file())
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
            Mismatch::PeerTooOld { .. } => {
                "is too old for this midgard: bifrost pull updates the boot files"
            }
            Mismatch::PeerTooNew { .. } => "is too new for this midgard: update midgard",
        };
        anyhow!("the guest agent {advice} (it speaks {mismatch})")
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
    fn descriptions() {
        let linux = Description::parse("kernel = \"vmlinuz\"").unwrap();
        assert_eq!(
            linux,
            Description {
                waypipe: None,
                ..Description::default()
            }
        );
        assert_eq!(linux.required(), ["vmlinuz", INIT_FILE, AGENT_FILE]);
        let managarm = Description::parse(
            r#"
            format = 1
            kernel = "eir-mb2"
            initrd = "initrd.cpio"
            agent = "image"
            console = "serial"
            share = false
            cmdline = "serial netserver.ip={guest-ip} netserver.subnet={netmask} netserver.gateway={gateway}"
            "#,
        )
        .unwrap();
        assert_eq!(managarm.required(), ["eir-mb2", "initrd.cpio"]);
        assert_eq!(managarm.agent, Agent::Image);
        assert_eq!(managarm.console, ConsoleKind::Serial);
        assert!(!managarm.share);
        assert_eq!(
            managarm.cmdline.as_deref(),
            Some(
                "serial netserver.ip=10.0.2.15 netserver.subnet=255.255.255.0 \
                 netserver.gateway=10.0.2.2"
            )
        );
        for invalid in [
            "",
            "kernel = \"../vmlinuz\"",
            "kernel = \"manifest.json\"",
            "kernel = \"k\"\ninitrd = \"i\"",
            "kernel = \"k\"\nagent = \"image\"\nwaypipe = \"w\"",
            "kernel = \"k\"\nagent = \"none\"",
            "kernel = \"k\"\nconsole = \"vga\"",
            "kernel = \"k\"\ncmdline = \"ip={host}\"",
            "kernel = \"k\"\ncmdline = \"a\\nb\"",
            "kernel = \"k\"\nfirmware = \"f\"",
        ] {
            assert!(Description::parse(invalid).is_err(), "{invalid:?}");
        }
        // A newer format, with settings that we do not know.
        let newer = format!("format = {}\nfirmware = \"f\"", FORMAT + 1);
        let error = Description::parse(&newer).unwrap_err().to_string();
        assert!(error.contains("needs a newer midgard"), "{error}");
    }

    /// The boot files that guest/build.py builds are those of Linux from before boot
    /// files had a description, but for the command line, which is their own.
    #[test]
    fn guest_boot_files() {
        let linux = Description::parse(include_str!("../../../guest/boot.toml")).unwrap();
        let cmdline = "reboot=k panic=-1 console=hvc0 \
                       ip=10.0.2.15::10.0.2.2:255.255.255.0:midgard:eth0:off:10.0.2.3";
        assert_eq!(
            linux,
            Description {
                cmdline: Some(cmdline.into()),
                ..Description::default()
            }
        );
    }

    #[test]
    fn placeholders() {
        assert_eq!(expand("").unwrap(), "");
        assert_eq!(
            expand("a={dns} b={dns},{guest-ip} c").unwrap(),
            "a=10.0.2.3 b=10.0.2.3,10.0.2.15 c"
        );
        // What is no placeholder stays.
        assert_eq!(expand("a={gateway} b={").unwrap(), "a=10.0.2.2 b={");
        assert!(expand("a={}").is_err());
    }

    #[test]
    fn loads() {
        let dir = crate::tar::tests::scratch("boot-files");
        let touch = |file: &str| std::fs::write(dir.join(file), b"").unwrap();
        let load = || BootFiles::load_from("test", dir.clone());
        // Without a description, they are Linux's.
        touch(KERNEL);
        let error = load().unwrap_err().to_string();
        assert!(
            error.contains("lack midgard-init and midgard-agent"),
            "{error}"
        );
        touch(INIT_FILE);
        touch(AGENT_FILE);
        let boot = load().unwrap();
        assert_eq!(boot.kernel(), dir.join(KERNEL));
        assert_eq!(boot.agent(), Some(dir.clone()));
        assert_eq!((boot.initrd(), boot.waypipe()), (None, None));
        touch(WAYPIPE);
        assert_eq!(load().unwrap().waypipe(), Some(dir.join(WAYPIPE)));
        // The description says what else they can be.
        std::fs::write(
            dir.join(DESCRIPTION),
            "kernel = \"eir\"\ninitrd = \"initrd.cpio\"\nagent = \"image\"\n",
        )
        .unwrap();
        let error = load().unwrap_err().to_string();
        assert!(error.contains("lack eir and initrd.cpio"), "{error}");
        touch("eir");
        touch("initrd.cpio");
        let boot = load().unwrap();
        assert_eq!(boot.kernel(), dir.join("eir"));
        assert_eq!(boot.initrd(), Some(dir.join("initrd.cpio")));
        assert_eq!((boot.agent(), boot.waypipe()), (None, None));
        std::fs::write(dir.join(DESCRIPTION), "kernel = 1").unwrap();
        let error = format!("{:#}", load().unwrap_err());
        assert!(error.contains("boot.toml"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
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
            assert!(error.contains("bifrost pull"), "{error}");
        }
    }
}
