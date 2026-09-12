use thiserror::Error;

use crate::archive::ArchiveError;
use crate::cli::CliError;
use crate::commands::install::InstallError;
use crate::integrity::IntegrityError;
use crate::linker::LinkError;
use crate::lockfile::LockfileError;
use crate::manifest::ManifestError;
use crate::range::RangeError;
use crate::registry::RegistryError;
use crate::resolver::ResolveError;
use crate::store::StoreError;
use crate::workspace::WorkspaceError;

#[derive(Debug, Error)]
pub enum JerkyError {
    #[error(transparent)]
    Cli(#[from] CliError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Integrity(#[from] IntegrityError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Link(#[from] LinkError),
    #[error(transparent)]
    Range(#[from] RangeError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Lockfile(#[from] LockfileError),
    #[error(transparent)]
    Install(#[from] InstallError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("could not determine the current directory")]
    Cwd(#[source] std::io::Error),
    #[error("could not determine the home directory for the package store")]
    NoHomeDirectory,
    #[error("no package.json found in {} or any parent directory", .0.display())]
    NoManifestAnywhere(std::path::PathBuf),
    #[error(
        "{} belongs to no workspace member (members: {})\n\
         cd into one of them, or add it to `workspaces` in the root package.json",
        .directory.display(),
        .members.join(", ")
    )]
    NotInAMember {
        directory: std::path::PathBuf,
        members: Vec<String>,
    },
}
