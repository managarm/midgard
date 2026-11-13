//! bifrost: runs commands, or a login shell, in midgard's VMs (see [`client`]).

use std::process::ExitCode;

use midgard_protocol::runtime::DEFAULT_VM;

mod boot;
mod cli;
mod client;
mod counters;
mod terminal;

use cli::Action;

/// Prints a message (like `eprintln!`) with bifrost's prefix.
macro_rules! log {
    ($($arg:tt)*) => { eprintln!("bifrost: {}", format_args!($($arg)*)) };
}
pub(crate) use log;

/// The exit code for bifrost's own failures, such that they aren't mistaken
/// for the command's exit code.
const EXIT_FAILURE: u8 = 255;

/// The VM instance that the command line names, or the default one.
fn vm(name: &Option<String>) -> &str {
    name.as_deref().unwrap_or(DEFAULT_VM)
}

fn main() -> ExitCode {
    let cli = match cli::Cli::try_parse_args(std::env::args_os()) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            // --help and --version are "errors" that go to stdout.
            return if error.use_stderr() {
                ExitCode::from(EXIT_FAILURE)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    let result = match &cli.action {
        None => client::run(vm(&cli.run.vm), &cli.run),
        Some(Action::Shutdown(args)) if args.all => client::shutdown_all().map(|()| 0),
        Some(Action::Shutdown(args)) => client::shutdown(vm(&args.name)).map(|()| 0),
        Some(Action::Create(args)) => client::create(args),
        Some(Action::Delete(args)) => client::delete(&args.name, args.yes),
        Some(Action::List) => client::list(),
        Some(Action::Pull(args)) => client::pull(args),
        Some(Action::Log(args)) => client::log(vm(&args.name), args.follow),
        Some(Action::Counters(args)) => counters::show(vm(&args.name), args).map(|()| 0),
        Some(Action::Boot(args)) => boot::boot(args).map(|()| 0),
    };
    terminal::restore();
    match result {
        // Exit codes of commands are 0 to 255 (128 + the signal for those that a signal
        // killed).
        Ok(code) => ExitCode::from(code as u8),
        Err(error) => {
            log!("{error:#}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}
