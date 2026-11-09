//! Templates, which VM instances are created from: a directory in midgard's templates
//! directory (see [`paths::templates_dir`]; guest/build.py installs them) with the
//! default settings of its instances (`template.toml`, see [`crate::settings`]) and the
//! disk that their disks start as a copy of (`disk.qcow2`, or `disk.qcow2.zst`, which is
//! compressed with zstd).

use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use midgard_protocol::control::TemplateInfo;

use crate::instance::validate_name;
use crate::paths;
use crate::settings::{self, Role, Settings};

/// The settings file in a template's directory.
const TEMPLATE_FILE: &str = "template.toml";
/// The disks that a template may have (exactly one of them).
const DISKS: [&str; 2] = ["disk.qcow2.zst", "disk.qcow2"];

/// A template.
pub struct Template {
    pub settings: Settings,
}

impl Template {
    /// Reads the template `name`.
    pub fn load(name: &str) -> Result<Self> {
        validate_name(name)?;
        let dir = paths::templates_dir().join(name);
        let path = dir.join(TEMPLATE_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                "no template {name} (in {}; guest/build.py install installs templates)",
                paths::templates_dir().display()
            ),
            Err(error) => return Err(error).with_context(|| format!("failed to read {path:?}")),
        };
        let settings =
            settings::parse(&text, Role::Template).with_context(|| format!("invalid {path:?}"))?;
        let disks: Vec<PathBuf> = DISKS
            .iter()
            .map(|disk| dir.join(disk))
            .filter(|disk| disk.exists())
            .collect();
        if disks.len() != 1 {
            bail!(
                "the template {name} needs exactly one of {} (in {})",
                DISKS.join(" and "),
                dir.display()
            );
        }
        Ok(Self { settings })
    }
}

/// The template `name` for `bifrost list`.
pub fn info(name: &str) -> TemplateInfo {
    let mut info = TemplateInfo {
        name: name.to_string(),
        description: None,
        needs_user: false,
        error: None,
    };
    match Template::load(name) {
        Ok(template) => {
            let guest = template.settings.guest;
            info.needs_user = guest.user_setup.is_some() && guest.user.is_none();
            info.description = template.settings.description;
        }
        Err(error) => info.error = Some(format!("{error:#}")),
    }
    info
}
