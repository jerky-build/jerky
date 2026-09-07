use std::process::ExitCode;

use clap::Parser;
use jerky::cli::{Cli, Command};
use jerky::error::JerkyError;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), JerkyError> {
    match cli.command {
        Command::Init => {
            let project_dir = std::env::current_dir().map_err(JerkyError::Cwd)?;
            let path = jerky::commands::init::init(&project_dir)?;
            println!("wrote {}", path.display());
            Ok(())
        }
        Command::Install { .. } => Err(JerkyError::NotImplemented("install")),
    }
}
