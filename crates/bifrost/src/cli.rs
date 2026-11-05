use std::ffi::OsString;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser};

/// Runs a command, or your login shell, in the VM.
///
/// The VM is started on demand and keeps running until it is shut down.
#[derive(Debug, Parser)]
#[command(
    version,
    override_usage = "\
bifrost [OPTIONS] [--] [COMMAND]...
       bifrost --create NAME [--template NAME] [--memory SIZE] [--disk-size SIZE]
       bifrost --delete NAME
       bifrost --list
       bifrost [-d NAME] --shutdown | --shutdown --all
       bifrost [-d NAME] --log [--follow]",
    args_override_self = true
)]
pub struct Cli {
    /// The VM [default: the configured default VM]
    #[arg(short = 'd', long, value_name = "NAME")]
    pub distro: Option<String>,

    #[command(flatten)]
    pub actions: Actions,

    #[command(flatten)]
    pub run: RunArgs,

    #[command(flatten)]
    pub create: CreateArgs,

    #[command(flatten)]
    pub modifiers: Modifiers,
}

#[derive(Debug, Args)]
#[group(id = "action", multiple = false)]
#[command(next_help_heading = "Actions")]
pub struct Actions {
    /// Create a new VM instance from a template
    #[arg(long, value_name = "NAME", conflicts_with = "distro")]
    create: Option<String>,

    /// Delete a VM instance and its disk
    #[arg(long, value_name = "NAME", conflicts_with = "distro")]
    delete: Option<String>,

    /// List VM instances and templates
    #[arg(short = 'l', long, conflicts_with = "distro")]
    list: bool,

    /// Stop the VM instance
    #[arg(long)]
    shutdown: bool,

    /// Show the VM instance's log
    #[arg(long)]
    log: bool,
}

#[derive(Debug, Args)]
#[group(id = "run", multiple = true, conflicts_with = "action")]
#[command(next_help_heading = "Running commands")]
pub struct RunArgs {
    /// The working directory in the guest [default: the current working directory
    /// through the shared directory mount]
    #[arg(short = 'C', long = "cd", value_name = "DIR")]
    pub cwd: Option<String>,

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

    /// The command and its arguments [default: your login shell]
    #[arg(trailing_var_arg = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
#[command(next_help_heading = "Creating VM instances")]
pub struct CreateArgs {
    /// The template to create the new VM instance from [default: the VM instance's name]
    #[arg(long, value_name = "NAME", requires = "create")]
    pub template: Option<String>,

    /// The VM instance's memory, e.g. 2G
    #[arg(long, value_name = "SIZE", value_parser = parse_size, requires = "create")]
    pub memory: Option<u64>,

    /// The size of the VM instance's disk, e.g. 64G
    #[arg(long, value_name = "SIZE", value_parser = parse_size, requires = "create")]
    pub disk_size: Option<u64>,
}

#[derive(Debug, Args)]
#[command(next_help_heading = "Other options")]
pub struct Modifiers {
    /// With --shutdown: stop all VM instances
    #[arg(long, requires = "shutdown", conflicts_with = "distro")]
    pub all: bool,

    /// With --log: keep printing as the console gets written to
    #[arg(short = 'f', long, requires = "log")]
    pub follow: bool,

    /// Answer questions with their defaults
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action<'a> {
    Run,
    /// Create the VM instance with this name.
    Create(&'a str),
    /// Delete the VM instance with this name.
    Delete(&'a str),
    List,
    Shutdown,
    Log,
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

    pub fn action(&self) -> Action<'_> {
        if let Some(name) = &self.actions.create {
            Action::Create(name)
        } else if let Some(name) = &self.actions.delete {
            Action::Delete(name)
        } else if self.actions.list {
            Action::List
        } else if self.actions.shutdown {
            Action::Shutdown
        } else if self.actions.log {
            Action::Log
        } else {
            Action::Run
        }
    }
}

impl RunArgs {
    pub fn env(&self) -> impl Iterator<Item = (&str, &str)> {
        self.env
            .chunks_exact(2)
            .map(|pair| (pair[0].as_str(), pair[1].as_str()))
    }

    /// Whether the command runs on a pseudo terminal (`None`: decide from stdin / stdout).
    #[expect(dead_code, reason = "the tty options are not wired up yet")]
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

fn parse_size(text: &str) -> Result<u64, String> {
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(digits);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("invalid size {text:?}"))?;
    let shift = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kib" => 10,
        "m" | "mib" => 20,
        "g" | "gib" => 30,
        "t" | "tib" => 40,
        _ => return Err(format!("invalid size suffix {suffix:?} (use K, M, G or T)")),
    };
    number
        .checked_mul(1 << shift)
        .ok_or_else(|| format!("size {text:?} is too large"))
}
