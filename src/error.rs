use thiserror::Error;

use crate::cli::CliError;
use crate::manifest::ManifestError;

#[derive(Debug, Error)]
pub enum JerkyError {
    #[error(transparent)]
    Cli(#[from] CliError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("could not determine the current directory")]
    Cwd(#[source] std::io::Error),
    #[error("`jerky {0}` is not implemented yet")]
    NotImplemented(&'static str),
}
