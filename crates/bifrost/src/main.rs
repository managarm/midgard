use std::process::ExitCode;

mod cli;

/// The exit code for bifrost's own failures, such that they aren't mistaken
/// for the command's exit code.
const EXIT_FAILURE: u8 = 255;

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
    eprintln!("bifrost: not implemented yet: {:?}", cli.action());
    ExitCode::from(EXIT_FAILURE)
}
