use thiserror::Error;

use crate::archive::ArchiveError;
use crate::cli::CliError;
use crate::commands::install::InstallError;
use crate::integrity::IntegrityError;
use crate::linker::LinkError;
use crate::manifest::ManifestError;
use crate::range::RangeError;
use crate::registry::RegistryError;
use crate::resolver::ResolveError;
use crate::store::StoreError;

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
    Install(#[from] InstallError),
    #[error("could not determine the current directory")]
    Cwd(#[source] std::io::Error),
    #[error("could not determine the home directory for the package store")]
    NoHomeDirectory,
}
