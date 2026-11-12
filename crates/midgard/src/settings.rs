//! The settings files of templates (`template.toml`, see [`crate::template`]) and VM
//! instances (`instance.toml`, see [`crate::instance`]). Both have the same grammar, such
//! that an instance's file starts as a copy of its template's (with its comments), but
//! some settings belong to one of them only (see [`Role`]):
//!
//! ```toml
//! # The format of the file (default: 1), see [`FORMAT`].
//! format = 1
//!
//! # The guest's RAM (default: 1G).
//! memory = "2G"
//!
//! # The boot files that midgard boots the guest with (default: linux), see
//! # [`crate::boot`].
//! boot-files = "linux"
//!
//! # About the template.
//! [template]
//! # What the template is (for people).
//! description = "Alpine Linux"
//! # The template that the instance was created from (only in instance files).
//! name = "alpine"
//!
//! [guest]
//! # The distribution's init, which the agent starts as PID 1 (default: none, the agent
//! # stays PID 1).
//! init = "/sbin/init"
//! # How sessions wait for the guest to boot: "none" (default), "systemd" (until
//! # `systemctl is-system-running --wait` returns) or "notify" (until the guest runs
//! # `/run/midgard/midgard-init ready`).
//! ready = "systemd"
//! # The user that sessions run as unless bifrost names one (default: root).
//! user = "alice"
//! # A command that the agent runs on every boot (as root, before the distribution's
//! # init) with the user's name as an additional argument, e.g., to create the user.
//! # Instance files need a user for it; a template's user is the default.
//! user-setup = ["/etc/midgard/user-setup"]
//! # How the agent runs the commands of sessions: "direct" (default: the agent runs
//! # them itself), "systemd" (as transient services of the user's systemd instance) or
//! # "wrapper" (through the command `wrapper`, which runs `{helper}` in a session of
//! # `{user}`).
//! launcher = "wrapper"
//! wrapper = ["runuser", "-l", "{user}", "-s", "{helper}"]
//! # Who starts the Wayland display for the guest's Wayland clients (with midgard's
//! # guest-waypipe): "agent" (default: the agent, for the default user), "systemd" (the
//! # users' systemd instances) or "guest" (the guest runs `/run/midgard/midgard-init
//! # waypipe` as the user itself).
//! display = "systemd"
//! ```

use anyhow::{Context, Result, bail};
use midgard_protocol::guest::{Display, GuestConfig, Launcher, Ready};
use midgard_protocol::size::{format_size_exact, parse_size};
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

/// The newest format of settings files that this midgard reads. Templates are distributed
/// apart from midgard, so a file that has what an older format lacks (e.g., a new
/// setting) says so (`format`), such that an older midgard can tell that it is too old.
pub const FORMAT: u32 = 1;

/// The longest name of a user in the guest.
const MAX_USER: usize = 32;

/// Which file the settings are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Template,
    Instance,
}

/// The settings of a template or a VM instance.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The template that the instance was created from.
    pub template: Option<String>,
    /// What the template is.
    pub description: Option<String>,
    /// The guest's RAM (in bytes).
    pub memory: Option<u64>,
    /// The boot files that midgard boots the guest with.
    pub boot_files: Option<String>,
    /// How the agent sets up the guest.
    pub guest: GuestConfig,
}

/// The contents of a settings file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct SettingsFile {
    #[expect(dead_code, reason = "checked before the rest of the file is read")]
    format: Option<u32>,
    memory: Option<String>,
    boot_files: Option<String>,
    #[serde(default)]
    template: TemplateSettings,
    #[serde(default)]
    guest: GuestSettings,
}

/// About the template.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateSettings {
    name: Option<String>,
    description: Option<String>,
}

/// How the agent sets up the guest (see [`GuestConfig`]).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct GuestSettings {
    init: Option<String>,
    ready: Option<ReadySetting>,
    user: Option<String>,
    user_setup: Option<Vec<String>>,
    launcher: Option<LauncherSetting>,
    wrapper: Option<Vec<String>>,
    display: Option<DisplaySetting>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ReadySetting {
    None,
    Systemd,
    Notify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LauncherSetting {
    Direct,
    Systemd,
    Wrapper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum DisplaySetting {
    Agent,
    Systemd,
    Guest,
}

impl GuestSettings {
    fn to_config(&self, role: Role) -> Result<GuestConfig> {
        let ready = match self.ready {
            None | Some(ReadySetting::None) => Ready::Immediately,
            Some(ReadySetting::Systemd) => Ready::Systemd,
            Some(ReadySetting::Notify) => Ready::Notify,
        };
        if let Some(init) = &self.init
            && !init.starts_with('/')
        {
            bail!("init must be an absolute path (not {init:?})");
        }
        if self.init.is_none() && ready != Ready::Immediately {
            bail!("ready needs init");
        }
        if let Some(user) = &self.user {
            validate_user(user)?;
        }
        if let Some(user_setup) = &self.user_setup {
            if user_setup.is_empty() {
                bail!("user-setup must not be empty");
            }
            if role == Role::Instance && self.user.is_none() {
                bail!("user-setup needs user");
            }
        }
        let launcher = match (self.launcher, &self.wrapper) {
            (Some(LauncherSetting::Wrapper), None) => bail!("launcher = \"wrapper\" needs wrapper"),
            (Some(LauncherSetting::Wrapper), Some(wrapper)) => {
                if !wrapper.iter().any(|arg| arg.contains("{helper}")) {
                    bail!("wrapper must run {{helper}}");
                }
                Launcher::Wrapper(wrapper.clone())
            }
            (_, Some(_)) => bail!("wrapper needs launcher = \"wrapper\""),
            (Some(LauncherSetting::Systemd), None) => {
                if self.init.is_none() {
                    bail!("launcher = \"systemd\" needs init");
                }
                Launcher::Systemd
            }
            (None | Some(LauncherSetting::Direct), None) => Launcher::Direct,
        };
        let display = match self.display {
            None | Some(DisplaySetting::Agent) => Display::Agent,
            Some(DisplaySetting::Systemd) => {
                if self.init.is_none() {
                    bail!("display = \"systemd\" needs init");
                }
                Display::Systemd
            }
            Some(DisplaySetting::Guest) => Display::Guest,
        };
        Ok(GuestConfig {
            init: self.init.clone(),
            ready,
            user: self.user.clone(),
            user_setup: self.user_setup.clone(),
            launcher,
            display,
            waypipe_debug: false,
        })
    }
}

/// Checks the name of a user in the guest (which user-setup gets as an argument): it
/// consists of letters, digits, `.`, `-` and `_` and does not start with `-`.
fn validate_user(user: &str) -> Result<()> {
    let valid = !user.is_empty()
        && user.len() <= MAX_USER
        && !user.starts_with('-')
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !valid {
        bail!(
            "invalid user {user:?} (use up to {MAX_USER} letters, digits, '.', '-' and '_', \
             not starting with '-')"
        );
    }
    Ok(())
}

/// The format of a settings file, which is all that we read of a file of a newer format.
#[derive(Deserialize)]
struct Header {
    format: Option<u32>,
}

/// Parses a settings file.
pub fn parse(text: &str, role: Role) -> Result<Settings> {
    let header: Header = toml::from_str(text)?;
    match header.format {
        Some(0) => bail!("format must be at least 1"),
        Some(format) if format > FORMAT => bail!(
            "it has the format {format}, which needs a newer midgard (this one reads up to \
             format {FORMAT})"
        ),
        _ => {}
    }
    let file: SettingsFile = toml::from_str(text)?;
    if role == Role::Template && file.template.name.is_some() {
        bail!("[template] name is only for instance files (a template's directory names it)");
    }
    let memory = file
        .memory
        .map(|memory| parse_size(&memory).map_err(anyhow::Error::msg))
        .transpose()
        .and_then(|memory| {
            memory.map(midgard_vmm::validate_memory).transpose()?;
            Ok(memory)
        })
        .context("invalid memory")?;
    if let Some(boot_files) = &file.boot_files {
        crate::instance::validate_name(boot_files).context("invalid boot-files")?;
    }
    let guest = file.guest.to_config(role).context("invalid [guest]")?;
    Ok(Settings {
        boot_files: file.boot_files,
        template: file.template.name,
        description: file.template.description,
        memory,
        guest,
    })
}

/// The table `key` of a settings file (a table or an inline table), which is created if
/// it is missing.
fn table<'a>(document: &'a mut DocumentMut, key: &str) -> Result<&'a mut dyn TableLike> {
    let item = document.entry(key).or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    item.as_table_like_mut()
        .with_context(|| format!("{key} must be a table"))
}

/// Sets `key` in `table` to `value`, keeping the comments of a setting that exists.
fn set(table: &mut dyn TableLike, key: &str, value: impl Into<Value>) {
    let mut value = value.into();
    match table.get_mut(key) {
        Some(item) => {
            if let Some(old) = item.as_value() {
                *value.decor_mut() = old.decor().clone();
            }
            *item = Item::Value(value);
        }
        None => {
            table.insert(key, Item::Value(value));
        }
    }
}

/// Sets a top-level setting. A new one goes before the tables, so it takes over the
/// comment at the top of the file (which says what the file is) from the first table:
/// the comment up to the last blank line in front of the table.
fn set_top_level(document: &mut DocumentMut, key: &str, value: impl Into<Value>) {
    let first_setting = !document.iter().any(|(_, item)| item.is_value());
    let exists = document.contains_key(key);
    set(document.as_table_mut(), key, value);
    if exists || !first_setting {
        return;
    }
    let first_table = document
        .iter_mut()
        .filter_map(|(_, item)| item.as_table_mut())
        .filter(|table| !table.is_implicit())
        .min_by_key(|table| table.position());
    let Some(first_table) = first_table else {
        return;
    };
    let prefix = first_table
        .decor()
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .unwrap_or_default()
        .to_string();
    // The lines up to the last blank one (with the line break in front of it).
    let mut split = None;
    let mut offset = 0;
    for line in prefix.split_inclusive('\n') {
        offset += line.len();
        if line.trim().is_empty() {
            split = Some(offset);
        }
    }
    let Some(split) = split else {
        return;
    };
    let (header, own) = prefix.split_at(split);
    first_table.decor_mut().set_prefix(format!("\n{own}"));
    if let Some(mut key) = document.key_mut(key) {
        key.leaf_decor_mut().set_prefix(header);
    }
}

/// Makes the instance file of a new instance from its template's file (keeping the
/// comments): it records the template's name and sets the instance's own settings.
pub fn instance_file(
    template_file: &str,
    template: &str,
    memory: Option<u64>,
    user: Option<&str>,
) -> Result<String> {
    let mut document: DocumentMut = template_file.parse()?;
    if let Some(memory) = memory {
        set_top_level(&mut document, "memory", format_size_exact(memory));
    }
    set(table(&mut document, "template")?, "name", template);
    if let Some(user) = user {
        set(table(&mut document, "guest")?, "user", user);
    }
    // Inline tables have no comments, but the spacing of their former last entry.
    for key in ["template", "guest"] {
        if let Some(table) = document.get_mut(key).and_then(Item::as_inline_table_mut) {
            table.fmt();
        }
    }
    let text = document.to_string();
    parse(&text, Role::Instance)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        assert_eq!(parse("", Role::Instance).unwrap(), Settings::default());
        let settings = parse(
            "memory = \"2G\"\n[template]\nname = \"alpine\"\ndescription = \"Alpine\"",
            Role::Instance,
        )
        .unwrap();
        assert_eq!(settings.memory, Some(2 << 30));
        assert_eq!(settings.template.as_deref(), Some("alpine"));
        assert_eq!(settings.description.as_deref(), Some("Alpine"));
        assert_eq!(settings.boot_files, None);
        let settings = parse("boot-files = \"managarm\"", Role::Template).unwrap();
        assert_eq!(settings.boot_files.as_deref(), Some("managarm"));
        for invalid in [
            "memory = \"2X\"",
            "memory = \"0\"",
            "memory = \"4097\"",
            "memory = 2",
            "unknown = 1",
            "[other]",
            "[template]\nversion = 1",
            "boot-files = \"../linux\"",
        ] {
            assert!(parse(invalid, Role::Instance).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn formats() {
        assert!(parse("format = 1\nmemory = \"2G\"", Role::Template).is_ok());
        assert!(parse("format = 0", Role::Template).is_err());
        // A newer format, with settings that we do not know.
        let newer = format!(
            "format = {}\nswap = \"1G\"\n[devices]\ngpu = true",
            FORMAT + 1
        );
        let error = parse(&newer, Role::Template).unwrap_err().to_string();
        assert!(error.contains("needs a newer midgard"), "{error}");
        // The instance file keeps the format of its template.
        let file = instance_file("format = 1\n[guest]\n", "alpine", Some(1 << 30), None).unwrap();
        assert!(file.contains("format = 1"), "{file}");
    }

    #[test]
    fn roles() {
        let user_setup = "[guest]\nuser-setup = [\"/setup\"]";
        assert!(parse(user_setup, Role::Template).is_ok());
        assert!(parse(user_setup, Role::Instance).is_err());
        let name = "[template]\nname = \"alpine\"";
        assert!(parse(name, Role::Instance).is_ok());
        assert!(parse(name, Role::Template).is_err());
    }

    #[test]
    fn instance_files() {
        let template = r#"# Alpine Linux.

[template]
# For people.
description = "Alpine"

[guest]
# The distribution's init.
init = "/sbin/init"
user-setup = ["/setup"]
"#;
        let text = instance_file(template, "alpine", Some(3 << 29), Some("alice")).unwrap();
        assert_eq!(
            text,
            r#"# Alpine Linux.

memory = "1536M"

[template]
# For people.
description = "Alpine"
name = "alpine"

[guest]
# The distribution's init.
init = "/sbin/init"
user-setup = ["/setup"]
user = "alice"
"#
        );
        let settings = parse(&text, Role::Instance).unwrap();
        assert_eq!(settings.template.as_deref(), Some("alpine"));
        assert_eq!(settings.guest.user.as_deref(), Some("alice"));

        // Settings that exist keep their comments; tables that the template does not have
        // are added.
        let text = instance_file(
            "# 1G\nmemory = \"1G\"\n",
            "plain",
            Some(2 << 30),
            Some("bob"),
        )
        .unwrap();
        assert!(text.starts_with("# 1G\nmemory = \"2G\"\n"), "{text}");
        let settings = parse(&text, Role::Instance).unwrap();
        assert_eq!(settings.memory, Some(2 << 30));
        assert_eq!(settings.template.as_deref(), Some("plain"));
        assert_eq!(settings.guest.user.as_deref(), Some("bob"));

        // Only the file's header moves to a new top-level setting; settings keep their
        // comments (also after the value) when they change.
        let text = instance_file(
            "# Header.\n\n# Template.\n[template]\n\n[guest]\nuser = \"x\" # Default.\n",
            "a",
            Some(1 << 30),
            Some("bob"),
        )
        .unwrap();
        assert_eq!(
            text,
            "# Header.\n\nmemory = \"1G\"\n\n# Template.\n[template]\nname = \"a\"\n\n\
             [guest]\nuser = \"bob\" # Default.\n"
        );
        let text = instance_file("# Template.\n[template]\n", "a", Some(1 << 30), None).unwrap();
        assert_eq!(
            text,
            "memory = \"1G\"\n# Template.\n[template]\nname = \"a\"\n"
        );

        // Inline tables and dotted keys.
        for template in [
            "guest = { init = \"/sbin/init\", user-setup = [\"/s\"] }\n",
            "guest.init = \"/sbin/init\"\nguest.user-setup = [\"/s\"]\n",
        ] {
            let text = instance_file(template, "a", None, Some("bob")).unwrap();
            assert!(!text.contains(" ,"), "{text}");
            let settings = parse(&text, Role::Instance).unwrap();
            assert_eq!(settings.guest.user.as_deref(), Some("bob"), "{text}");
        }

        // The instance needs a user for user-setup.
        assert!(instance_file(template, "alpine", None, None).is_err());
        assert!(instance_file(template, "alpine", None, Some("-x")).is_err());
        assert!(instance_file("template = 1", "alpine", None, None).is_err());
    }

    #[test]
    fn guest() {
        let guest = |text: &str| parse(text, Role::Instance).map(|settings| settings.guest);
        let config = guest(
            r#"
            [guest]
            init = "/sbin/init"
            ready = "notify"
            "#,
        )
        .unwrap();
        assert_eq!(config.init.as_deref(), Some("/sbin/init"));
        assert_eq!(config.ready, Ready::Notify);
        assert_eq!(config.launcher, Launcher::Direct);
        let config = guest(
            r#"
            [guest]
            user = "alice"
            launcher = "wrapper"
            wrapper = ["runuser", "-l", "{user}", "-s", "{helper}"]
            display = "guest"
            "#,
        )
        .unwrap();
        assert_eq!(config.user.as_deref(), Some("alice"));
        assert_eq!(config.display, Display::Guest);
        assert!(matches!(config.launcher, Launcher::Wrapper(argv) if argv.len() == 5));
        for invalid in [
            "[guest]\ninit = \"sbin/init\"",
            "[guest]\nready = \"systemd\"",
            "[guest]\nready = \"soon\"",
            "[guest]\nlauncher = \"systemd\"",
            "[guest]\nlauncher = \"wrapper\"",
            "[guest]\nlauncher = \"wrapper\"\nwrapper = [\"su\"]",
            "[guest]\nwrapper = [\"{helper}\"]",
            "[guest]\nuser = \"alice\"\nuser-setup = []",
            "[guest]\nuser = \"a/b\"",
            "[guest]\nuser-setup = [\"/setup\"]",
            "[guest]\ndisplay = \"systemd\"",
            "[guest]\nunknown = 1",
        ] {
            assert!(guest(invalid).is_err(), "{invalid:?}");
        }
    }
}
