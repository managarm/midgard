//! Pulling: midgard installs boot files (see [`crate::boot`]) and templates (see
//! [`crate::template`]) from a channel, a URL or a directory with `channel.json`, which
//! lists them (see [`Channel`]; guest/build.py writes it), and their files. CI builds
//! them and publishes them as midgard's default channel ([`DEFAULT_CHANNEL`], see
//! guest/publish.py).
//!
//! Each item is installed next to where it belongs (in a directory whose name starts with
//! a dot, which is no item) and moved there once it is complete; what uses the installed
//! files meanwhile holds [`reading`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError, RwLock, RwLockReadGuard, TryLockError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use midgard_protocol::control::{Pull, PullEvent, PullItem, Pulled};
use midgard_protocol::version::{Mismatch, Versions};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::boot;
use crate::instance::{Instance, validate_name};
use crate::template::{self, Template};
use crate::{log, paths, settings, tar};

/// The channel that midgard pulls from unless bifrost (or MIDGARD_CHANNEL) names one: the
/// repository's GitHub Pages, which list the files of the newest guest release (see
/// .github/workflows/guest-release.yml).
const DEFAULT_CHANNEL: &str = "https://managarm.github.io/midgard/";
/// The file of a channel that lists what the channel has.
const CHANNEL_FILE: &str = "channel.json";
/// The newest format of [`CHANNEL_FILE`] that this midgard reads.
const FORMAT: u32 = 1;
/// The largest [`CHANNEL_FILE`] that we read.
const MAX_CHANNEL_FILE: u64 = 1 << 20;

/// How long a server may take to accept a connection and to answer a request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a server may send nothing during a download. (A download as a whole may take
/// as long as it needs.)
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// How often bifrost hears about a download.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// Held by whoever reads installed boot files or templates (shared) or replaces them
/// (exclusively).
static FILES: RwLock<()> = RwLock::new(());
/// Held by the pull that runs.
static PULLING: Mutex<()> = Mutex::new(());

/// Keeps the installed boot files and templates as they are while it lives.
pub fn reading() -> RwLockReadGuard<'static, ()> {
    FILES.read().unwrap_or_else(PoisonError::into_inner)
}

/// What a channel has (`channel.json`). Newer channels may say more (which we ignore);
/// one that an older midgard could not use has a newer `format`.
#[derive(Debug, Deserialize)]
struct Channel {
    /// Boot files, by name.
    #[serde(default)]
    boot: BTreeMap<String, ChannelBoot>,
    /// Templates, by name.
    #[serde(default)]
    templates: BTreeMap<String, ChannelTemplate>,
}

/// The format of a channel, which is all that we read of a channel of a newer format.
#[derive(Debug, Deserialize)]
struct ChannelFormat {
    format: u32,
}

/// A file of a channel.
#[derive(Debug, Deserialize)]
struct ChannelFile {
    /// Where the file is: a URL, or a name next to `channel.json`.
    url: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct ChannelBoot {
    /// The versions of the guest protocol that the agent speaks.
    protocol: Option<Versions>,
    /// The files, by their names in the boot files' directory.
    files: BTreeMap<String, ChannelFile>,
}

#[derive(Debug, Deserialize)]
struct ChannelTemplate {
    /// The template's tar file: its directory, by the template's name.
    #[serde(flatten)]
    file: ChannelFile,
    /// The format of its settings file (see [`settings::FORMAT`]).
    #[serde(default = "first_format")]
    settings: u32,
    /// The boot files that its instances boot with.
    boot: Option<String>,
}

fn first_format() -> u32 {
    1
}

impl Channel {
    fn parse(text: &[u8]) -> Result<Self> {
        let format = serde_json::from_slice::<ChannelFormat>(text)?.format;
        if format > FORMAT {
            bail!(
                "it has the format {format}, which needs a newer midgard (this one reads up to \
                 format {FORMAT})"
            );
        }
        let mut channel: Self = serde_json::from_slice(text)?;
        // What we cannot name is not for us (e.g., a newer midgard allows more names): we
        // leave it out instead of rejecting the channel.
        channel.boot.retain(|name, boot| {
            let valid =
                validate_name(name).is_ok() && boot.files.keys().all(|file| file_name(file));
            if !valid {
                log!("ignoring the channel's boot files {name:?}, whose names are invalid");
            }
            valid
        });
        channel.templates.retain(|name, _| {
            let valid = validate_name(name).is_ok();
            if !valid {
                log!("ignoring the channel's template {name:?}, whose name is invalid");
            }
            valid
        });
        Ok(channel)
    }
}

/// Whether `name` can be a file of boot files: a plain name that is none of our own (those
/// start with a dot, or are the manifest).
fn file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name != boot::MANIFEST
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Where a channel or one of its files is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Location {
    Url(String),
    Path(PathBuf),
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Url(url) => url.fmt(f),
            Self::Path(path) => path.display().fmt(f),
        }
    }
}

fn is_url(text: &str) -> bool {
    text.starts_with("https://") || text.starts_with("http://")
}

impl Location {
    fn new(text: &str) -> Self {
        if is_url(text) {
            Self::Url(text.to_string())
        } else {
            Self::Path(text.into())
        }
    }

    /// The location of `url`, which a file at this location (a channel) names: a URL, or a
    /// name next to that file.
    fn file(&self, url: &str) -> Result<Self> {
        if is_url(url) {
            return Ok(Self::Url(url.to_string()));
        }
        if url.is_empty() || url.starts_with('.') || url.contains(['/', '\\', ':', '?', '#']) {
            bail!("{url:?} is neither a URL nor the name of a file of the channel");
        }
        Ok(match self {
            Self::Url(base) => Self::Url(format!("{}/{url}", base.trim_end_matches('/'))),
            Self::Path(base) => Self::Path(base.join(url)),
        })
    }
}

/// A channel that we pull from.
struct Source {
    base: Location,
    agent: ureq::Agent,
    channel: Channel,
}

impl Source {
    /// Reads the channel at `from` (default: MIDGARD_CHANNEL, else [`DEFAULT_CHANNEL`]).
    fn open(from: Option<&str>) -> Result<Self> {
        let from = from
            .map(str::to_string)
            .or_else(|| std::env::var("MIDGARD_CHANNEL").ok())
            .unwrap_or_else(|| DEFAULT_CHANNEL.to_string());
        let base = Location::new(&from);
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(CONNECT_TIMEOUT))
            .timeout_per_read(Some(READ_TIMEOUT))
            .user_agent(concat!("midgard/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        let location = base.file(CHANNEL_FILE)?;
        let channel = (|| {
            let mut text = Vec::new();
            read(&agent, &location)?
                .take(MAX_CHANNEL_FILE + 1)
                .read_to_end(&mut text)?;
            if text.len() as u64 > MAX_CHANNEL_FILE {
                bail!("it is too large");
            }
            Channel::parse(&text)
        })()
        .with_context(|| format!("cannot use the channel {location}"))?;
        Ok(Self {
            base,
            agent,
            channel,
        })
    }

    /// Downloads `file` (which people know as `name`) to the new file `to` and checks it.
    fn download(
        &self,
        file: &ChannelFile,
        name: &str,
        to: &Path,
        report: &mut Report,
    ) -> Result<()> {
        let location = self.base.file(&file.url)?;
        (|| {
            let mut reader = read(&self.agent, &location)?;
            let mut output = File::create_new(to)?;
            let mut digest = Sha256::new();
            let mut buffer = vec![0u8; 1 << 16];
            let mut done = 0u64;
            let mut reported = None;
            loop {
                if reported.is_none_or(|at: Instant| at.elapsed() >= PROGRESS_INTERVAL) {
                    reported = Some(Instant::now());
                    report(PullEvent::Downloading {
                        file: name.to_string(),
                        done,
                        total: file.size,
                    })?;
                }
                let count = match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                };
                done += count as u64;
                if done > file.size {
                    bail!("it is larger than the channel says");
                }
                digest.update(&buffer[..count]);
                output.write_all(&buffer[..count])?;
            }
            let sha256: String = digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            if done != file.size || !sha256.eq_ignore_ascii_case(&file.sha256) {
                bail!("it is not the file that the channel lists (its size or SHA-256 differs)");
            }
            output.sync_all()?;
            report(PullEvent::Downloading {
                file: name.to_string(),
                done,
                total: file.size,
            })
        })()
        .with_context(|| format!("failed to download {location}"))
    }
}

/// Opens what is at `location`.
fn read(agent: &ureq::Agent, location: &Location) -> Result<Box<dyn Read>> {
    Ok(match location {
        Location::Url(url) => Box::new(agent.get(url).call()?.into_body().into_reader()),
        Location::Path(path) => Box::new(File::open(path)?),
    })
}

/// Tells bifrost what happens; fails once bifrost is gone (which stops the pull).
type Report<'a> = dyn FnMut(PullEvent) -> Result<()> + 'a;

/// A new, empty directory for what becomes the item `name` of `dir` (which is no item
/// itself, see [`paths::names`]).
fn stage(dir: &Path, name: &str) -> Result<PathBuf> {
    let stage = dir.join(format!(".{name}.partial"));
    if stage.exists() {
        std::fs::remove_dir_all(&stage).with_context(|| format!("failed to delete {stage:?}"))?;
    }
    std::fs::create_dir_all(&stage).with_context(|| format!("failed to create {stage:?}"))?;
    Ok(stage)
}

/// Makes the directory `new` the item `name` of `dir`, in place of the one that is there
/// (which nothing may read meanwhile).
fn install(new: &Path, dir: &Path, name: &str) -> Result<()> {
    let target = dir.join(name);
    let old = dir.join(format!(".{name}.old"));
    // (Its entries are durable before it takes the place of the old one.)
    sync_dir(new);
    let _files = FILES.write().unwrap_or_else(PoisonError::into_inner);
    let replaced = (|| {
        if old.exists() {
            std::fs::remove_dir_all(&old)?;
        }
        if target.exists() {
            std::fs::rename(&target, &old)?;
        }
        if let Err(error) = std::fs::rename(new, &target) {
            let _ = std::fs::rename(&old, &target);
            return Err(error);
        }
        Ok(())
    })();
    replaced.with_context(|| format!("failed to install {target:?}"))?;
    sync_dir(dir);
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Syncs a directory, such that the entries in it are durable (best effort: not all file
/// systems can sync directories).
fn sync_dir(path: &Path) {
    if let Ok(dir) = File::open(path) {
        let _ = dir.sync_all();
    }
}

/// Copies the file `from` to the new file `to` and syncs it.
fn copy_synced(from: &Path, to: &Path) -> io::Result<()> {
    let mut output = File::create_new(to)?;
    io::copy(&mut File::open(from)?, &mut output)?;
    output.sync_all()
}

/// Cleans up after a pull that midgard stopped in the middle of, in `dir`: an item that
/// [`install`] moved out of the way without getting to put the new one in its place is
/// moved back, and the rest of what pulls left behind is removed. No pull may run
/// meanwhile.
fn recover(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let file_name = entry.file_name();
        let name = file_name
            .to_str()
            .and_then(|name| name.strip_prefix('.')?.strip_suffix(".old"));
        if let Some(name) = name
            && validate_name(name).is_ok()
            && !dir.join(name).exists()
        {
            log!("restoring {:?}, which a pull was replacing", dir.join(name));
            if let Err(error) = std::fs::rename(entry.path(), dir.join(name)) {
                log!("failed to restore {:?}: {error}", dir.join(name));
            }
        }
    }
    paths::remove_leftovers(dir, &[".partial", ".old"]);
}

/// Cleans up after a pull that midgard stopped in the middle of. No pull may run
/// meanwhile.
pub fn remove_leftovers() {
    for dir in [paths::boot_root(), paths::templates_dir()] {
        recover(&dir);
    }
}

/// Installs the boot files `name` of the channel in `root` (see [`paths::boot_root`]):
/// the files of the channel that are not installed already.
fn pull_boot(
    source: &Source,
    root: &Path,
    name: &str,
    entry: &ChannelBoot,
    report: &mut Report,
) -> Result<()> {
    if let Some(protocol) = entry.protocol
        && let Err(mismatch) = Versions::OURS.negotiate(protocol)
    {
        // (No pull helps here, unlike with installed boot files, see `boot::negotiate`.)
        let advice = match mismatch {
            Mismatch::PeerTooOld { .. } => {
                "are too old for this midgard: the channel needs a newer guest release"
            }
            Mismatch::PeerTooNew { .. } => "are too new for this midgard: update midgard",
        };
        bail!("the channel's boot files {name} {advice} (their agent speaks {mismatch})");
    }
    let dir = root.join(name);
    // The channel may lack files that are installed (e.g., a directory in which only the
    // agent was built): we keep those.
    let mut installed = boot::read_manifest(&dir).unwrap_or_default();
    installed.files.retain(|file, _| dir.join(file).is_file());
    let mut wanted = installed.clone();
    wanted.protocol = entry.protocol.or(installed.protocol);
    for (file, info) in &entry.files {
        wanted
            .files
            .insert(file.clone(), info.sha256.to_ascii_lowercase());
    }
    let missing: Vec<_> = boot::REQUIRED
        .into_iter()
        .filter(|file| !wanted.files.contains_key(*file))
        .collect();
    if !missing.is_empty() {
        bail!(
            "the channel's boot files {name} lack {}, which is not installed either",
            missing.join(" and ")
        );
    }
    let item = PullItem::BootFiles(name.to_string());
    if installed == wanted {
        return report(PullEvent::UpToDate(item));
    }
    let stage = stage(root, name)?;
    let result = (|| {
        for (file, sha256) in &wanted.files {
            let to = stage.join(file);
            match entry.files.get(file) {
                Some(info) if installed.files.get(file) != Some(sha256) => {
                    source.download(info, file, &to, report)?;
                }
                _ => {
                    copy_synced(&dir.join(file), &to)
                        .with_context(|| format!("failed to write {to:?}"))?;
                }
            }
        }
        boot::write_manifest(&stage, &wanted)?;
        install(&stage, root, name)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    result?;
    log!("installed the boot files {name}");
    report(PullEvent::Installed(item))
}

/// Installs the template `name` of the channel in `root` (see [`paths::templates_dir`])
/// unless it is installed already.
fn pull_template(
    source: &Source,
    root: &Path,
    name: &str,
    entry: &ChannelTemplate,
    report: &mut Report,
) -> Result<()> {
    if entry.settings > settings::FORMAT {
        bail!(
            "the channel's template {name} needs a newer midgard (its settings have the format \
             {}; this midgard reads up to format {})",
            entry.settings,
            settings::FORMAT
        );
    }
    let sha256 = entry.file.sha256.to_ascii_lowercase();
    let item = PullItem::Template(name.to_string());
    if template::installed_from(&root.join(name)).as_deref() == Some(&sha256) {
        return report(PullEvent::UpToDate(item));
    }
    let stage = stage(root, name)?;
    let result = (|| {
        let file = format!("{name}.tar");
        let archive = stage.join(&file);
        source.download(&entry.file, &file, &archive, report)?;
        let unpacked = stage.join("unpacked");
        let skipped = File::open(&archive)
            .map_err(anyhow::Error::from)
            .and_then(|archive| tar::extract(archive, &unpacked))
            .with_context(|| format!("cannot unpack {file}"))?;
        let new = template::locate(&unpacked)
            .and_then(|new| Template::load_from(name, &new).map(|_| new))
            .with_context(|| {
                // What we skipped may be what the template lacks.
                let skipped: Vec<_> = skipped
                    .iter()
                    .filter(|member| {
                        member
                            .path
                            .file_name()
                            .is_some_and(template::is_template_file)
                    })
                    .map(|member| member.to_string())
                    .collect();
                if skipped.is_empty() {
                    format!("{file} has no valid template")
                } else {
                    format!(
                        "{file} has no valid template (midgard skipped {})",
                        skipped.join(", ")
                    )
                }
            })?;
        template::record_installed_from(&new, &sha256)?;
        install(&new, root, name)
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result?;
    log!("installed the template {name}");
    report(PullEvent::Installed(item))
}

/// The boot files that are installed and those that the installed templates and the VM
/// instances boot with.
fn boot_files_in_use() -> BTreeSet<String> {
    let mut names: BTreeSet<String> = paths::names(&paths::boot_root()).into_iter().collect();
    let templates = paths::names(&paths::templates_dir())
        .into_iter()
        .filter_map(|name| Template::load(&name).ok())
        .map(|template| template.settings);
    let instances = paths::names(&paths::instances_dir())
        .into_iter()
        .filter_map(|name| Instance::load(&name).ok())
        .map(|instance| instance.settings);
    for settings in templates.chain(instances) {
        names.insert(settings.boot_files.unwrap_or_else(|| boot::DEFAULT.into()));
    }
    names
}

/// Installs or updates boot files and templates from a channel (see [`Pull`]).
pub fn pull(request: &Pull, report: &mut Report) -> Result<Pulled> {
    let _pulling = match PULLING.try_lock() {
        Ok(pulling) => pulling,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => bail!("midgard is pulling already"),
    };
    for name in &request.templates {
        validate_name(name)?;
    }
    let source = Source::open(request.from.as_deref())?;
    let channel = &source.channel;

    let mut templates: BTreeSet<&str> = request.templates.iter().map(String::as_str).collect();
    for name in &templates {
        if !channel.templates.contains_key(*name) {
            let names: Vec<_> = channel.templates.keys().map(String::as_str).collect();
            bail!(
                "the channel has no template {name} (it has {})",
                if names.is_empty() {
                    "none".into()
                } else {
                    names.join(", ")
                }
            );
        }
    }
    let installed = paths::names(&paths::templates_dir());
    if request.all {
        templates.extend(
            installed
                .iter()
                .map(String::as_str)
                .filter(|name| channel.templates.contains_key(*name)),
        );
    }

    // The boot files that the new templates need must be in the channel; of the others,
    // we update those that it has.
    let mut boot_files = boot_files_in_use();
    for name in &templates {
        let needed = channel.templates[*name].boot.as_deref();
        let needed = needed.unwrap_or(boot::DEFAULT);
        if !channel.boot.contains_key(needed) {
            bail!("the channel lacks the boot files {needed}, which the template {name} needs");
        }
        boot_files.insert(needed.to_string());
    }
    if boot_files.is_empty() {
        boot_files.insert(boot::DEFAULT.to_string());
    }
    for name in &boot_files {
        if let Some(entry) = channel.boot.get(name) {
            pull_boot(&source, &paths::boot_root(), name, entry, report)?;
        }
    }
    for name in templates {
        let entry = &channel.templates[name];
        pull_template(&source, &paths::templates_dir(), name, entry, report)?;
    }

    let installed = paths::names(&paths::templates_dir());
    let available = channel
        .templates
        .keys()
        .filter(|name| !installed.contains(name))
        .cloned()
        .collect();
    Ok(Pulled {
        installed,
        available,
    })
}

#[cfg(test)]
mod tests {
    use midgard_protocol::version::PROTOCOL;

    use super::*;
    use crate::tar::tests::{archive, finish, member, scratch};

    fn sha256(data: &[u8]) -> String {
        let digest = Sha256::digest(data);
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Writes a file of a channel; returns its entry for channel.json.
    fn publish(channel: &Path, name: &str, data: &[u8]) -> String {
        std::fs::write(channel.join(name), data).unwrap();
        format!(
            r#""url": "{name}", "sha256": "{}", "size": {}"#,
            sha256(data),
            data.len()
        )
    }

    /// A channel in `dir` with the boot files linux (whose agent is `agent`) and the
    /// template alpine.
    fn channel(dir: &Path, agent: &[u8], protocol: u32) -> Source {
        let channel = dir.join("channel");
        let _ = std::fs::remove_dir_all(&channel);
        std::fs::create_dir_all(&channel).unwrap();
        let template = archive(&[
            ("alpine", b'5', b""),
            (
                "alpine/template.toml",
                b'0',
                b"[template]\ndescription = \"Alpine\"\n",
            ),
            ("alpine/disk.qcow2", b'0', &[1; 2000]),
        ]);
        let text = format!(
            r#"{{
                "format": 1,
                "comment": "what a newer channel may add",
                "boot": {{"linux": {{
                    "protocol": {{"oldest": {protocol}, "current": {protocol}}},
                    "files": {{"vmlinuz": {{{}}}, "midgard-init": {{{}}}}}
                }}}},
                "templates": {{"alpine": {{{}, "description": "Alpine", "boot": "linux"}}}}
            }}"#,
            publish(&channel, "vmlinuz", b"kernel"),
            publish(&channel, "midgard-init", agent),
            publish(&channel, "alpine.tar", &template),
        );
        std::fs::write(channel.join(CHANNEL_FILE), text).unwrap();
        Source::open(Some(channel.to_str().unwrap())).unwrap()
    }

    #[test]
    fn pulls() {
        let dir = scratch("pull");
        let (boot_root, templates) = (dir.join("boot"), dir.join("templates"));
        let mut events = Vec::new();
        let mut report = |event| {
            events.push(event);
            Ok(())
        };
        let linux = PullItem::BootFiles("linux".into());
        let alpine = PullItem::Template("alpine".into());

        let source = channel(&dir, b"agent 1", PROTOCOL);
        let boot_entry = &source.channel.boot["linux"];
        let template_entry = &source.channel.templates["alpine"];
        pull_boot(&source, &boot_root, "linux", boot_entry, &mut report).unwrap();
        pull_template(&source, &templates, "alpine", template_entry, &mut report).unwrap();
        let installed = boot_root.join("linux");
        assert_eq!(std::fs::read(installed.join("vmlinuz")).unwrap(), b"kernel");
        assert_eq!(
            std::fs::read(installed.join("midgard-init")).unwrap(),
            b"agent 1"
        );
        let manifest = boot::read_manifest(&installed).unwrap();
        assert_eq!(manifest.protocol, boot_entry.protocol);
        assert_eq!(manifest.files["vmlinuz"], sha256(b"kernel"));
        assert!(Template::load_from("alpine", &templates.join("alpine")).is_ok());
        assert_eq!(
            template::installed_from(&templates.join("alpine")).unwrap(),
            template_entry.file.sha256
        );
        assert!(events.contains(&PullEvent::Installed(linux.clone())));
        assert!(events.contains(&PullEvent::Installed(alpine.clone())));
        assert!(events.contains(&PullEvent::Downloading {
            file: "midgard-init".into(),
            done: 7,
            total: 7,
        }));
        assert!(events.contains(&PullEvent::Downloading {
            file: "alpine.tar".into(),
            done: template_entry.file.size,
            total: template_entry.file.size,
        }));

        // Nothing changed.
        events.clear();
        let mut report = |event| {
            events.push(event);
            Ok(())
        };
        pull_boot(&source, &boot_root, "linux", boot_entry, &mut report).unwrap();
        pull_template(&source, &templates, "alpine", template_entry, &mut report).unwrap();
        assert_eq!(
            events,
            [
                PullEvent::UpToDate(linux.clone()),
                PullEvent::UpToDate(alpine)
            ]
        );

        // A new agent: only it is downloaded.
        events.clear();
        let mut report = |event| {
            events.push(event);
            Ok(())
        };
        let source = channel(&dir, b"agent 2", PROTOCOL);
        let boot_entry = &source.channel.boot["linux"];
        pull_boot(&source, &boot_root, "linux", boot_entry, &mut report).unwrap();
        assert_eq!(
            std::fs::read(installed.join("midgard-init")).unwrap(),
            b"agent 2"
        );
        assert_eq!(std::fs::read(installed.join("vmlinuz")).unwrap(), b"kernel");
        assert!(events.iter().all(|event| !matches!(
            event,
            PullEvent::Downloading { file, .. } if file == "vmlinuz"
        )));
        assert_eq!(events.last(), Some(&PullEvent::Installed(linux)));

        // An agent that is too new for us stays out, as does a file that is not the one
        // that the channel lists.
        let mut report = |_| Ok(());
        let source = channel(&dir, b"agent 3", PROTOCOL + 1);
        let boot_entry = &source.channel.boot["linux"];
        let error = pull_boot(&source, &boot_root, "linux", boot_entry, &mut report).unwrap_err();
        assert!(format!("{error:#}").contains("update midgard"), "{error:#}");
        let source = channel(&dir, b"agent 4", PROTOCOL);
        std::fs::write(dir.join("channel/midgard-init"), b"agent X").unwrap();
        let boot_entry = &source.channel.boot["linux"];
        let error = pull_boot(&source, &boot_root, "linux", boot_entry, &mut report).unwrap_err();
        assert!(format!("{error:#}").contains("SHA-256"), "{error:#}");
        assert_eq!(
            std::fs::read(installed.join("midgard-init")).unwrap(),
            b"agent 2"
        );
        assert_eq!(paths::names(&boot_root), ["linux"]);
        assert!(!boot_root.join(".linux.partial").exists());

        // A channel with the agent only keeps the kernel that is installed.
        let agent = publish(&dir.join("channel"), "midgard-init", b"agent 6");
        let text = format!(
            r#"{{"format": 1, "boot": {{"linux": {{"files": {{"midgard-init": {{{agent}}}}}}}}}}}"#
        );
        std::fs::write(dir.join("channel").join(CHANNEL_FILE), text).unwrap();
        let channel_dir = dir.join("channel");
        let source = Source::open(Some(channel_dir.to_str().unwrap())).unwrap();
        let boot_entry = &source.channel.boot["linux"];
        // (Which does not do where there is no kernel yet.)
        let error = pull_boot(
            &source,
            &dir.join("empty"),
            "linux",
            boot_entry,
            &mut report,
        );
        let error = format!("{:#}", error.unwrap_err());
        assert!(error.contains("lack vmlinuz"), "{error}");
        assert!(!dir.join("empty/linux").exists());
        pull_boot(&source, &boot_root, "linux", boot_entry, &mut report).unwrap();
        assert_eq!(
            std::fs::read(installed.join("midgard-init")).unwrap(),
            b"agent 6"
        );
        assert_eq!(std::fs::read(installed.join("vmlinuz")).unwrap(), b"kernel");
        let manifest = boot::read_manifest(&installed).unwrap();
        assert_eq!(manifest.files["vmlinuz"], sha256(b"kernel"));
        assert!(manifest.protocol.is_some());

        // bifrost went away.
        let mut report = |_| bail!("gone");
        let source = channel(&dir, b"agent 5", PROTOCOL);
        let boot_entry = &source.channel.boot["linux"];
        assert!(pull_boot(&source, &boot_root, "linux", boot_entry, &mut report).is_err());
        assert_eq!(
            std::fs::read(installed.join("midgard-init")).unwrap(),
            b"agent 6"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn template_failures() {
        let dir = scratch("pull-template");
        let templates = dir.join("templates");
        let mut report = |_| Ok(());
        let source = channel(&dir, b"agent", PROTOCOL);
        let entry = &source.channel.templates["alpine"];
        pull_template(&source, &templates, "alpine", entry, &mut report).unwrap();
        let installed = template::installed_from(&templates.join("alpine"));
        let fails = |archive: &[u8], corrupt: bool| {
            let channel = dir.join("channel");
            let file = publish(&channel, "alpine.tar", archive);
            if corrupt {
                std::fs::write(channel.join("alpine.tar"), b"something else").unwrap();
            }
            let text = format!(r#"{{"format": 1, "templates": {{"alpine": {{{file}}}}}}}"#);
            std::fs::write(channel.join(CHANNEL_FILE), text).unwrap();
            let source = Source::open(Some(channel.to_str().unwrap())).unwrap();
            let entry = &source.channel.templates["alpine"];
            let mut report = |_| Ok(());
            let error = pull_template(&source, &templates, "alpine", entry, &mut report);
            // The template that was installed stays, and nothing is left behind.
            assert_eq!(
                template::installed_from(&templates.join("alpine")),
                installed
            );
            assert_eq!(paths::names(&templates), ["alpine"]);
            assert_eq!(std::fs::read_dir(&templates).unwrap().count(), 1);
            format!("{:#}", error.unwrap_err())
        };

        // A disk that is a symbolic link, which we skip (and say so).
        let settings = b"[template]\ndescription = \"Alpine\"\n";
        let link = finish(&[
            member("alpine/template.toml", b'0', "", settings),
            member("alpine/disk.qcow2", b'2', "/dev/sda", b""),
        ]);
        let error = fails(&link, false);
        assert!(
            error.contains("\"alpine/disk.qcow2\", which is a symbolic link"),
            "{error}"
        );
        // No template, a broken archive and one that is not what the channel lists.
        let error = fails(&archive(&[("alpine/disk.qcow2", b'0', b"disk")]), false);
        assert!(error.contains("no valid template"), "{error}");
        assert!(!error.contains("skipped"), "{error}");
        let error = fails(&link[..700], false);
        assert!(error.contains("cannot unpack"), "{error}");
        let error = fails(&link, true);
        assert!(error.contains("SHA-256"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn channels() {
        assert!(Channel::parse(br#"{"format": 1}"#).is_ok());
        let newer = br#"{"format": 2, "boot": "something else"}"#;
        let error = Channel::parse(newer).unwrap_err().to_string();
        assert!(error.contains("needs a newer midgard"), "{error}");
        assert!(Channel::parse(b"{}").is_err());
        // What has names that we do not allow is left out.
        let file = r#"{"url": "a", "sha256": "", "size": 0}"#;
        let text = format!(
            r#"{{"format": 1,
                "boot": {{
                    "../linux": {{"files": {{}}}},
                    "escapes": {{"files": {{"../vmlinuz": {file}}}}},
                    "manifest": {{"files": {{"manifest.json": {file}}}}},
                    "linux": {{"files": {{"vmlinuz": {file}}}}}
                }},
                "templates": {{"a/b": {file}, "ubuntu-24.04": {file}, "alpine": {file}}}
            }}"#
        );
        let channel = Channel::parse(text.as_bytes()).unwrap();
        assert_eq!(channel.boot.keys().collect::<Vec<_>>(), ["linux"]);
        assert_eq!(channel.templates.keys().collect::<Vec<_>>(), ["alpine"]);
    }

    #[test]
    fn recovers() {
        let dir = scratch("pull-recover");
        for name in [
            "linux",
            ".linux.partial",
            ".alpine.old",
            ".debian.old",
            "debian",
        ] {
            std::fs::create_dir(dir.join(name)).unwrap();
        }
        std::fs::write(dir.join(".alpine.old/disk"), b"old").unwrap();
        recover(&dir);
        // alpine was being replaced and is back; debian was replaced already.
        assert_eq!(paths::names(&dir), ["alpine", "debian", "linux"]);
        assert_eq!(std::fs::read(dir.join("alpine/disk")).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn locations() {
        let url = Location::new("https://example.org/guest/");
        assert_eq!(
            url.file("vmlinuz").unwrap(),
            Location::Url("https://example.org/guest/vmlinuz".into())
        );
        assert_eq!(
            url.file("http://example.org/other").unwrap(),
            Location::Url("http://example.org/other".into())
        );
        let dir = Location::new("/srv/guest");
        assert_eq!(
            dir.file("alpine.tar").unwrap(),
            Location::Path("/srv/guest/alpine.tar".into())
        );
        for invalid in [
            "",
            "../vmlinuz",
            "/etc/passwd",
            "a/b",
            ".hidden",
            "file:///x",
        ] {
            assert!(dir.file(invalid).is_err(), "{invalid:?}");
            assert!(url.file(invalid).is_err(), "{invalid:?}");
        }
    }
}
