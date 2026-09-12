use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use jerky::cli::{Cli, Command};
use jerky::error::JerkyError;
use jerky::resolver::ImporterPath;
use jerky::workspace::{Warning, Workspace};

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
            // Deliberately not workspace-aware: `jerky init` in a
            // subdirectory does not register the new package as a member,
            // because editing `workspaces` is the user's decision and
            // silently rewriting the root manifest would be surprising.
            let path = jerky::commands::init::init(&project_dir)?;
            println!("wrote {}", path.display());
            Ok(())
        }
        Command::Install { spec } => {
            let spec = jerky::cli::parse_package_spec(&spec)?;

            let root = Workspace::find_root(&project_dir)
                .ok_or_else(|| JerkyError::NoManifestAnywhere(project_dir.clone()))?;
            let workspace = Workspace::discover(&root)?;
            for warning in workspace.warnings() {
                match warning {
                    Warning::PatternMatchedNothing(pattern) => {
                        eprintln!("warning: workspace pattern `{pattern}` matched no package");
                    }
                }
            }

            let importer = importer_for(&workspace, &project_dir)?;

            let store = jerky::store::Store::new(store_root()?);
            let registry = jerky::registry::HttpRegistry::new();
            let installed =
                jerky::commands::install::install(&workspace, &importer, &store, &registry, &spec)?;
            println!(
                "added {}@{} to {importer}",
                installed.name, installed.version
            );
            Ok(())
        }
    }
}

/// Which importer the user is standing in.
///
/// `member_for` answers with the nearest enclosing member, which for a
/// directory belonging to no other member is the root. That answer is right
/// when the root is the only member — there is nowhere else it could have
/// meant — and ambiguous when there are others: standing in `tools/scripts`
/// could mean the root, or could mean the user is lost. Ambiguity needs
/// alternatives to exist, so the error is raised only where alternatives do.
fn importer_for(workspace: &Workspace, dir: &Path) -> Result<ImporterPath, JerkyError> {
    let member = workspace
        .member_for(dir)
        .ok_or_else(|| JerkyError::NoManifestAnywhere(dir.to_path_buf()))?;

    let standing_in_its_own_directory = dir
        .canonicalize()
        .is_ok_and(|resolved| resolved == member.path);
    if member.importer.is_root() && !standing_in_its_own_directory && workspace.members().len() > 1
    {
        return Err(JerkyError::NotInAMember {
            directory: dir.to_path_buf(),
            members: workspace
                .members()
                .keys()
                .map(ImporterPath::to_string)
                .collect(),
        });
    }

    Ok(member.importer.clone())
}

/// `main` is the only place allowed to read `$HOME`; everything below it takes
/// paths as parameters so tests never touch a developer's real store.
fn store_root() -> Result<PathBuf, JerkyError> {
    let home = dirs::home_dir().ok_or(JerkyError::NoHomeDirectory)?;
    Ok(home.join(".jerky").join("store"))
}
