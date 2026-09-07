//! A staging directory that cleans itself up unless explicitly kept.
//!
//! Both the store and the linker publish a directory tree by building it under
//! a staging name and renaming it into place, so that a half-built tree is
//! never visible at the path that means "complete". They share this guard so
//! the two cannot drift apart in how they clean up — which they had, one using
//! `Drop` and the other unwinding by hand, leaving the hand-rolled one to
//! strand debris on a panic.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const STAGING_DIR: &str = ".staging";

/// A filesystem operation that failed, with the path it was applied to.
pub(crate) struct StagingError {
    pub path: PathBuf,
    pub source: std::io::Error,
}

/// A name unique among concurrent processes and within this one, without
/// pulling in a random number generator.
fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), nanos, n)
}

/// A staging directory that deletes itself on drop unless kept.
///
/// Hand-rolled rather than using `tempfile::TempDir` because the directory has
/// to be *moved* on success, and disabling `TempDir`'s cleanup for that is an
/// API that has churned across tempfile versions.
pub(crate) struct StagingDir {
    path: PathBuf,
    keep: bool,
}

impl StagingDir {
    /// Create a uniquely named staging directory under `<parent>/.staging/`.
    ///
    /// `parent` is the directory the finished tree will be renamed into, so
    /// staging always shares a filesystem with its destination and the rename
    /// is atomic rather than failing with `EXDEV`.
    pub(crate) fn create_under(parent: &Path) -> Result<Self, StagingError> {
        let staging_root = parent.join(STAGING_DIR);
        std::fs::create_dir_all(&staging_root).map_err(|source| StagingError {
            path: staging_root.clone(),
            source,
        })?;

        let path = staging_root.join(unique_suffix());
        std::fs::create_dir(&path).map_err(|source| StagingError {
            path: path.clone(),
            source,
        })?;

        Ok(Self { path, keep: false })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Give up ownership of the directory, so dropping this guard no longer
    /// deletes it. Called immediately before the rename that publishes it.
    pub(crate) fn keep(&mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
