use thiserror::Error;

use crate::archive::ArchiveError;
use crate::cli::CliError;
use crate::integrity::IntegrityError;
use crate::manifest::ManifestError;

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
    #[error("could not determine the current directory")]
    Cwd(#[source] std::io::Error),
    #[error("`jerky {0}` is not implemented yet")]
    NotImplemented(&'static str),
}
