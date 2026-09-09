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
    let project_dir = std::env::current_dir().map_err(JerkyError::Cwd)?;

    match cli.command {
        Command::Init => {
            let path = jerky::commands::init::init(&project_dir)?;
            println!("wrote {}", path.display());
            Ok(())
        }
        Command::Install { spec } => {
            let spec = jerky::cli::parse_package_spec(&spec)?;
            let store = jerky::store::Store::new(store_root()?);
            let registry = jerky::registry::HttpRegistry::new();
            let installed =
                jerky::commands::install::install(&project_dir, &store, &registry, &spec)?;
            println!("added {}@{}", installed.name, installed.version);
            Ok(())
        }
    }
}

/// `main` is the only place allowed to read `$HOME`; everything below it takes
/// paths as parameters so tests never touch a developer's real store.
fn store_root() -> Result<std::path::PathBuf, JerkyError> {
    let home = dirs::home_dir().ok_or(JerkyError::NoHomeDirectory)?;
    Ok(home.join(".jerky").join("store"))
}
