use thiserror::Error;

use crate::archive::ArchiveError;
use crate::cli::CliError;
use crate::integrity::IntegrityError;
use crate::linker::LinkError;
use crate::manifest::ManifestError;
use crate::registry::RegistryError;
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
    Registry(#[from] RegistryError),
    #[error("could not determine the current directory")]
    Cwd(#[source] std::io::Error),
    #[error("`jerky {0}` is not implemented yet")]
    NotImplemented(&'static str),
}
