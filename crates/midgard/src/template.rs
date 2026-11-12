//! Templates, which VM instances are created from: a directory in midgard's templates
//! directory (see [`paths::templates_dir`]; guest/build.py installs them) with the
//! default settings of its instances (`template.toml`, see [`crate::settings`]) and the
//! disk that their disks start as a copy of (`disk.qcow2`, or `disk.qcow2.zst`, which is
//! compressed with zstd).

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use midgard_protocol::control::TemplateInfo;

use crate::instance::validate_name;
use crate::settings::{self, Role, Settings};
use crate::{paths, zstd};

/// The settings file in a template's directory.
const TEMPLATE_FILE: &str = "template.toml";
/// The disks that a template may have (exactly one of them).
const DISKS: [&str; 2] = ["disk.qcow2.zst", "disk.qcow2"];
/// The extension of a compressed disk.
const COMPRESSED: &str = "zst";
/// The file in a template's directory with the SHA-256 of the tar file that the template
/// was installed from.
const INSTALLED_FROM: &str = "installed-from";

/// A template.
pub struct Template {
    /// Its settings file.
    pub text: String,
    pub settings: Settings,
    /// Its disk.
    disk: PathBuf,
}

impl Template {
    /// Reads the template `name`.
    pub fn load(name: &str) -> Result<Self> {
        validate_name(name)?;
        Self::load_from(name, &paths::templates_dir().join(name))
    }

    /// Reads the template `name` from its directory `dir`.
    pub fn load_from(name: &str, dir: &Path) -> Result<Self> {
        let path = dir.join(TEMPLATE_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                "no template {name} (in {}; guest/build.py install installs templates)",
                dir.display()
            ),
            Err(error) => return Err(error).with_context(|| format!("failed to read {path:?}")),
        };
        let settings =
            settings::parse(&text, Role::Template).with_context(|| format!("invalid {path:?}"))?;
        let mut disks: Vec<PathBuf> = DISKS
            .iter()
            .map(|disk| dir.join(disk))
            .filter(|disk| disk.is_file())
            .collect();
        if disks.len() != 1 {
            bail!(
                "the template {name} needs exactly one of {} (in {})",
                DISKS.join(" and "),
                dir.display()
            );
        }
        Ok(Self {
            text,
            settings,
            disk: disks.pop().unwrap(),
        })
    }

    /// Opens the template's disk.
    pub fn open_disk(&self) -> Result<TemplateDisk> {
        let file =
            File::open(&self.disk).with_context(|| format!("failed to open {:?}", self.disk))?;
        Ok(TemplateDisk {
            file,
            path: self.disk.clone(),
        })
    }
}

/// A template's disk. It stays readable if the template is replaced meanwhile (see
/// [`crate::pull`]).
pub struct TemplateDisk {
    file: File,
    /// Where the disk was when we opened it.
    path: PathBuf,
}

impl TemplateDisk {
    /// Copies the disk to the new file `to` (decompressing it if it is compressed) and
    /// syncs it.
    pub fn copy_to(self, to: &Path) -> Result<()> {
        let compressed = self.path.extension().is_some_and(|e| e == COMPRESSED);
        let copied = (|| {
            let mut output = File::create_new(to)?;
            if compressed {
                decompress(self.file, &mut output)?;
            } else {
                io::copy(&mut BufReader::new(self.file), &mut output)?;
            }
            output.sync_all()?;
            anyhow::Ok(())
        })();
        copied.with_context(|| format!("failed to copy {:?} to {to:?}", self.path))
    }
}

/// Whether `name` is the name of a file of a template that it needs (its settings, or a
/// disk).
pub fn is_template_file(name: &OsStr) -> bool {
    name == TEMPLATE_FILE || DISKS.iter().any(|disk| name == *disk)
}

/// The directory in `dir` with a template: `dir` itself, or the only directory in it with
/// one, whatever its name (e.g., in an unpacked tar file, see [`crate::tar`]).
pub fn locate(dir: &Path) -> Result<PathBuf> {
    if dir.join(TEMPLATE_FILE).is_file() {
        return Ok(dir.to_path_buf());
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("failed to read {dir:?}"))? {
        let path = entry
            .with_context(|| format!("failed to read {dir:?}"))?
            .path();
        if path.join(TEMPLATE_FILE).is_file() {
            found.push(path);
        }
    }
    match found.len() {
        0 => bail!("there is no {TEMPLATE_FILE}, neither at the top nor in a directory there"),
        1 => Ok(found.pop().unwrap()),
        _ => bail!("there are several directories with a {TEMPLATE_FILE}"),
    }
}

/// The SHA-256 of the tar file that the template in `dir` was installed from, if it says.
pub fn installed_from(dir: &Path) -> Option<String> {
    let marker = std::fs::read_to_string(dir.join(INSTALLED_FROM)).ok()?;
    Some(marker.trim().to_string())
}

/// Records the SHA-256 of the tar file that the template in `dir` is installed from.
pub fn record_installed_from(dir: &Path, sha256: &str) -> Result<()> {
    let path = dir.join(INSTALLED_FROM);
    File::create(&path)
        .and_then(|mut file| {
            file.write_all(format!("{sha256}\n").as_bytes())?;
            file.sync_all()
        })
        .with_context(|| format!("failed to write {path:?}"))
}

/// Decompresses a file that zstd compressed.
fn decompress(input: File, output: &mut File) -> Result<()> {
    let mut output = BufWriter::new(output);
    io::copy(&mut zstd::Frames::new(BufReader::new(input)), &mut output)?;
    output.flush()?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The templates that guest/build.py builds.
    const TEMPLATES: [(&str, &str); 3] = [
        (
            "alpine",
            include_str!("../../../guest/images/alpine/template.toml"),
        ),
        (
            "arch",
            include_str!("../../../guest/images/arch/template.toml"),
        ),
        (
            "debian",
            include_str!("../../../guest/images/debian/template.toml"),
        ),
    ];

    #[test]
    fn guest_templates() {
        for (name, text) in TEMPLATES {
            let template = settings::parse(text, Role::Template).expect(name);
            assert!(template.description.is_some(), "{name}");
            let instance = settings::instance_file(text, name, None, Some("alice")).expect(name);
            let instance = settings::parse(&instance, Role::Instance).expect(name);
            assert_eq!(instance.template.as_deref(), Some(name));
            assert_eq!(instance.guest.user.as_deref(), Some("alice"));
        }
    }

    #[test]
    fn locates() {
        let dir = crate::tar::tests::scratch("template-locate");
        let touch = |path: &str| {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        };
        // At the top, in the only directory with one (whatever else there is), or nowhere.
        touch("top/template.toml");
        touch("top/sub/template.toml");
        assert_eq!(locate(&dir.join("top")).unwrap(), dir.join("top"));
        touch("nested/my-alpine/template.toml");
        touch("nested/._my-alpine");
        touch("nested/other/file");
        let found = locate(&dir.join("nested")).unwrap();
        assert_eq!(found, dir.join("nested/my-alpine"));
        touch("several/a/template.toml");
        touch("several/b/template.toml");
        assert!(locate(&dir.join("several")).is_err());
        touch("none/a/b/template.toml");
        assert!(locate(&dir.join("none")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn needs_files() {
        let dir = crate::tar::tests::scratch("template-files");
        std::fs::write(
            dir.join(TEMPLATE_FILE),
            "[template]\ndescription = \"Alpine\"\n",
        )
        .unwrap();
        std::fs::create_dir(dir.join("disk.qcow2")).unwrap();
        let error = Template::load_from("alpine", &dir).err().unwrap();
        assert!(error.to_string().contains("needs exactly one"), "{error}");
        std::fs::write(dir.join("disk.qcow2.zst"), b"").unwrap();
        assert!(Template::load_from("alpine", &dir).is_ok());
        assert!(is_template_file(OsStr::new("disk.qcow2")));
        assert!(!is_template_file(OsStr::new("disk.raw")));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
