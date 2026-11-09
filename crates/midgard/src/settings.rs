//! The settings files of templates (`template.toml`, see [`crate::template`]) and VM
//! instances (`instance.toml`, see [`crate::instance`]). Both have the same grammar, such
//! that an instance's file starts as a copy of its template's (with its comments), but
//! some settings belong to one of them only (see [`Role`]):
//!
//! ```toml
//! # The guest's RAM (default: 1G).
//! memory = "2G"
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
use midgard_protocol::size::parse_size;
use serde::Deserialize;

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
    /// How the agent sets up the guest.
    pub guest: GuestConfig,
}

/// The contents of a settings file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    memory: Option<String>,
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

/// Parses a settings file.
pub fn parse(text: &str, role: Role) -> Result<Settings> {
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
    let guest = file.guest.to_config(role).context("invalid [guest]")?;
    Ok(Settings {
        template: file.template.name,
        description: file.template.description,
        memory,
        guest,
    })
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
        for invalid in [
            "memory = \"2X\"",
            "memory = \"0\"",
            "memory = \"4097\"",
            "memory = 2",
            "unknown = 1",
            "[other]",
            "[template]\nversion = 1",
        ] {
            assert!(parse(invalid, Role::Instance).is_err(), "{invalid:?}");
        }
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
            "[guest]\nuser-setup = [\"/setup\"]",
            "[guest]\ndisplay = \"systemd\"",
            "[guest]\nunknown = 1",
        ] {
            assert!(guest(invalid).is_err(), "{invalid:?}");
        }
    }
}
