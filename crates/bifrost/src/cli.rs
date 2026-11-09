use std::ffi::OsString;
use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use midgard_protocol::machine::Disk;
use midgard_protocol::size::parse_size;

/// Runs a command, or your login shell, in the VM.
///
/// The VM is started on demand and keeps running until it is shut down. To run a
/// command that has the name of an action, put `--` in front of it.
#[derive(Debug, Parser)]
#[command(
    version,
    args_conflicts_with_subcommands = true,
    disable_help_subcommand = true,
    subcommand_value_name = "ACTION",
    subcommand_help_heading = "Actions",
    args_override_self = true
)]
pub struct Cli {
    /// What to do instead of running a command.
    #[command(subcommand)]
    pub action: Option<Action>,

    #[command(flatten)]
    pub run: RunArgs,
}

#[derive(Debug, Subcommand)]
pub enum Action {
    /// Create a new VM instance from a template
    Create(CreateArgs),

    /// Delete a VM instance and its disk
    Delete(DeleteArgs),

    /// List VM instances and templates
    List,

    /// Stop a VM instance
    Shutdown(ShutdownArgs),

    /// Show a VM instance's log
    Log(LogArgs),

    /// Boot a VM in the foreground, with its console on stdin / stdout (for debugging;
    /// Ctrl-A x stops the VM, twice exits right away)
    Boot(BootArgs),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// The VM instance [default: alpine]
    #[arg(long, value_name = "NAME")]
    pub vm: Option<String>,

    /// The working directory in the guest [default: the user's home directory]
    #[arg(short = 'C', long = "cd", value_name = "DIR")]
    pub cwd: Option<String>,

    /// The user in the guest [default: the VM instance's default user]
    #[arg(short = 'u', long, value_name = "NAME")]
    pub user: Option<String>,

    /// Set an environment variable, can be provided multiple times
    #[arg(
        short = 'e',
        long = "env",
        num_args = 2,
        value_names = ["KEY", "VALUE"],
        allow_hyphen_values = true
    )]
    env: Vec<String>,

    /// Run the command on a pseudo terminal [default: if stdin and stdout are terminals]
    #[arg(short = 't', long, overrides_with = "no_tty")]
    tty: bool,

    /// Run the command on pipes
    #[arg(short = 'T', long, overrides_with = "tty")]
    no_tty: bool,

    /// Fail if the VM instance isn't running instead of starting it
    #[arg(long)]
    pub no_start: bool,

    /// Answer questions with their defaults
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// The command and its arguments [default: your login shell]
    #[arg(trailing_var_arg = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// The name of the new VM instance
    pub name: String,

    /// The template to create the new VM instance from [default: the VM instance's name]
    #[arg(long, value_name = "NAME")]
    pub template: Option<String>,

    /// The user that sessions run as, if the template creates one [default: ask, with
    /// your user name as the answer's default]
    #[arg(short = 'u', long, value_name = "NAME")]
    pub user: Option<String>,

    /// The VM instance's memory, e.g. 2G
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub memory: Option<u64>,

    /// Answer questions with their defaults
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct DeleteArgs {
    /// The VM instance to delete
    pub name: String,

    /// Answer questions with their defaults
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct ShutdownArgs {
    /// The VM instance [default: alpine]
    pub name: Option<String>,

    /// Stop all VM instances
    #[arg(long, conflicts_with = "name")]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct LogArgs {
    /// The VM instance [default: alpine]
    pub name: Option<String>,

    /// Keep printing as the console gets written to
    #[arg(short = 'f', long)]
    pub follow: bool,
}

#[derive(Debug, Args)]
pub struct BootArgs {
    /// The guest kernel (a bzImage)
    #[arg(long, value_name = "FILE")]
    pub kernel: PathBuf,

    /// An initrd (instead of midgard-init)
    #[arg(long, value_name = "FILE", conflicts_with = "agent")]
    pub initrd: Option<PathBuf>,

    /// Boot midgard-init from FILE as the guest's agent
    #[arg(long, value_name = "FILE")]
    pub agent: Option<PathBuf>,

    /// Replace the base kernel parameters
    #[arg(long, value_name = "ARGS", allow_hyphen_values = true)]
    pub cmdline: Option<String>,

    /// Add a kernel parameter, can be provided multiple times
    #[arg(long, value_name = "ARG", allow_hyphen_values = true)]
    pub append: Vec<String>,

    /// The VM's memory, e.g. 2G
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub memory: Option<u64>,

    /// The number of vCPUs [default: the host's CPUs, up to 8]
    #[arg(long, value_name = "N")]
    pub cpus: Option<u32>,

    /// The guest's console (serial also shows the kernel's messages before it finds
    /// its virtio devices)
    #[arg(long, value_name = "KIND", value_enum, default_value_t = Console::Virtio)]
    pub console: Console,

    /// No network interface
    #[arg(long)]
    pub no_network: bool,

    /// The host directory in /mnt/managarm [default: /]
    #[arg(long, value_name = "DIR", conflicts_with = "no_share")]
    pub share: Option<PathBuf>,

    /// No shared directory
    #[arg(long)]
    pub no_share: bool,

    /// A disk image (raw or qcow2), can be provided multiple times (the first one is
    /// the root file system)
    #[arg(long, value_name = "FILE[,ro]", value_parser = parse_disk)]
    pub disk: Vec<Disk>,
}

/// See [`BootArgs::console`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Console {
    Virtio,
    Serial,
}

impl Cli {
    pub fn try_parse_args<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let cli = Self::try_parse_from(args)?;
        for (key, _) in cli.run.env() {
            if key.is_empty() || key.contains('=') {
                return Err(Self::command().error(
                    ErrorKind::ValueValidation,
                    format!("invalid environment variable name {key:?}"),
                ));
            }
        }
        Ok(cli)
    }
}

impl RunArgs {
    pub fn env(&self) -> impl Iterator<Item = (&str, &str)> {
        // clap takes the values in pairs (num_args = 2), so there is no remainder.
        let (pairs, _) = self.env.as_chunks::<2>();
        pairs
            .iter()
            .map(|[key, value]| (key.as_str(), value.as_str()))
    }

    /// Whether the command runs on a pseudo terminal (`None`: decide from stdin / stdout).
    pub fn tty(&self) -> Option<bool> {
        if self.tty {
            Some(true)
        } else if self.no_tty {
            Some(false)
        } else {
            None
        }
    }
}

/// Parses `FILE[,ro]`.
fn parse_disk(text: &str) -> Result<Disk, String> {
    let (path, read_only) = match text.strip_suffix(",ro") {
        Some(path) => (path, true),
        None => (text, false),
    };
    if path.is_empty() {
        return Err("empty disk path".into());
    }
    Ok(Disk {
        path: path.into(),
        read_only,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_args(std::iter::once("bifrost").chain(args.iter().copied()))
    }

    #[test]
    fn run() {
        let cli = parse(&["--vm", "dev", "ls", "-la", "--color"]).unwrap();
        assert!(cli.action.is_none());
        assert_eq!(cli.run.vm.as_deref(), Some("dev"));
        assert_eq!(cli.run.command, ["ls", "-la", "--color"]);
        // The VM is not an argument (that would be the command).
        assert_eq!(parse(&["dev", "ls"]).unwrap().run.command, ["dev", "ls"]);
        assert!(parse(&["-d", "dev", "ls"]).is_err());
    }

    #[test]
    fn run_shell() {
        let cli = parse(&[]).unwrap();
        assert!(cli.action.is_none());
        assert!(cli.run.command.is_empty());
    }

    #[test]
    fn run_command_named_like_an_action() {
        let cli = parse(&["--", "list", "-v"]).unwrap();
        assert!(cli.action.is_none());
        assert_eq!(cli.run.command, ["list", "-v"]);

        let cli = parse(&["help"]).unwrap();
        assert_eq!(cli.run.command, ["help"]);

        let cli = parse(&["-C", "/tmp", "boot"]).unwrap();
        assert!(cli.action.is_none());
        assert_eq!(cli.run.command, ["boot"]);
    }

    #[test]
    fn run_options_conflict_with_actions() {
        assert!(parse(&["list", "-C", "/tmp"]).is_err());
        assert!(parse(&["create", "alpine", "--no-start"]).is_err());
    }

    #[test]
    fn env() {
        let cli = parse(&["-e", "A", "-1", "env"]).unwrap();
        assert_eq!(cli.run.env().collect::<Vec<_>>(), [("A", "-1")]);
        assert_eq!(cli.run.command, ["env"]);
        assert!(parse(&["-e", "A=B", "C", "env"]).is_err());
    }

    #[test]
    fn boot() {
        let cli = parse(&[
            "boot", "--kernel", "bzImage", "--append", "quiet", "--append", "-v", "--memory", "2G",
            "--cpus", "4",
        ])
        .unwrap();
        let Some(Action::Boot(boot)) = cli.action else {
            panic!("not boot: {:?}", cli.action);
        };
        assert_eq!(boot.kernel, PathBuf::from("bzImage"));
        assert_eq!(boot.append, ["quiet", "-v"]);
        assert_eq!(boot.memory, Some(2 << 30));
        assert_eq!(boot.cpus, Some(4));
    }

    #[test]
    fn boot_disks() {
        let cli = parse(&[
            "boot",
            "--kernel",
            "bzImage",
            "--disk",
            "root.img",
            "--disk",
            "data.img,ro",
        ])
        .unwrap();
        let Some(Action::Boot(boot)) = cli.action else {
            panic!("not boot: {:?}", cli.action);
        };
        assert_eq!(
            boot.disk,
            [
                Disk {
                    path: "root.img".into(),
                    read_only: false
                },
                Disk {
                    path: "data.img".into(),
                    read_only: true
                }
            ]
        );
        assert!(parse(&["boot", "--kernel", "bzImage", "--disk", ",ro"]).is_err());
    }

    #[test]
    fn boot_agent() {
        let cli = parse(&["boot", "--kernel", "bzImage", "--agent", "midgard-init"]).unwrap();
        let Some(Action::Boot(boot)) = cli.action else {
            panic!("not boot: {:?}", cli.action);
        };
        assert_eq!(boot.agent, Some(PathBuf::from("midgard-init")));
        assert!(parse(&["boot", "--kernel", "k", "--agent", "a", "--initrd", "i"]).is_err());
    }

    #[test]
    fn boot_share() {
        let cli = parse(&["boot", "--kernel", "bzImage", "--share", "/home"]).unwrap();
        let Some(Action::Boot(boot)) = cli.action else {
            panic!("not boot: {:?}", cli.action);
        };
        assert_eq!(boot.share, Some(PathBuf::from("/home")));
        assert!(!boot.no_share);
        let cli = parse(&["boot", "--kernel", "bzImage", "--no-share"]).unwrap();
        let Some(Action::Boot(boot)) = cli.action else {
            panic!("not boot: {:?}", cli.action);
        };
        assert!(boot.no_share);
        assert!(parse(&["boot", "--kernel", "k", "--share", "/", "--no-share"]).is_err());
    }

    #[test]
    fn boot_console() {
        let cli = parse(&["boot", "--kernel", "bzImage", "--console", "serial"]).unwrap();
        let Some(Action::Boot(boot)) = cli.action else {
            panic!("not boot: {:?}", cli.action);
        };
        assert_eq!(boot.console, Console::Serial);
        assert!(parse(&["boot", "--kernel", "k", "--console", "vga"]).is_err());
    }

    #[test]
    fn boot_needs_a_kernel() {
        assert!(parse(&["boot"]).is_err());
    }

    #[test]
    fn options_belong_to_their_action() {
        assert!(parse(&["--kernel", "bzImage"]).is_err());
        assert!(parse(&["--memory", "2G"]).is_err());
        assert!(parse(&["create", "alpine", "--kernel", "bzImage"]).is_err());
        assert!(parse(&["list", "--all"]).is_err());
    }

    #[test]
    fn create() {
        let cli = parse(&["create", "dev", "--template", "debian", "--memory", "512M"]).unwrap();
        let Some(Action::Create(create)) = cli.action else {
            panic!("not create: {:?}", cli.action);
        };
        assert_eq!(create.name, "dev");
        assert_eq!(create.template.as_deref(), Some("debian"));
        assert_eq!(create.memory, Some(512 << 20));
        assert_eq!(create.user, None);
        let cli = parse(&["create", "-y", "-u", "alice", "alpine"]).unwrap();
        let Some(Action::Create(create)) = cli.action else {
            panic!("not create: {:?}", cli.action);
        };
        assert_eq!(create.user.as_deref(), Some("alice"));
        assert!(create.yes);
        assert!(parse(&["create"]).is_err());
    }

    #[test]
    fn shutdown() {
        let cli = parse(&["shutdown", "dev"]).unwrap();
        let Some(Action::Shutdown(shutdown)) = cli.action else {
            panic!("not shutdown: {:?}", cli.action);
        };
        assert_eq!(shutdown.name.as_deref(), Some("dev"));
        assert!(!shutdown.all);
        assert!(parse(&["shutdown", "--all"]).is_ok());
        assert!(parse(&["shutdown", "--all", "dev"]).is_err());
    }

    #[test]
    fn log() {
        let cli = parse(&["log", "-f", "dev"]).unwrap();
        let Some(Action::Log(log)) = cli.action else {
            panic!("not log: {:?}", cli.action);
        };
        assert_eq!(log.name.as_deref(), Some("dev"));
        assert!(log.follow);
        let cli = parse(&["log"]).unwrap();
        let Some(Action::Log(log)) = cli.action else {
            panic!("not log: {:?}", cli.action);
        };
        assert_eq!(log.name, None);
    }
}
