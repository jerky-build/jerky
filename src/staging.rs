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

/// Put a freshly created staging directory at 0o755.
///
/// `create_dir` derives its mode from the process umask, and the store renames
/// this directory into place as the entry root — so under a permissive umask
/// the store would hold a group- or world-writable directory, and another user
/// could add or replace files inside a package every project on the machine
/// imports. `archive::extract` normalises what it writes *into* the directory
/// but never the directory itself, which the caller owns; this is that caller.
#[cfg(unix)]
fn set_traversable(path: &Path) -> Result<(), StagingError> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|source| {
        StagingError {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_traversable(_path: &Path) -> Result<(), StagingError> {
    Ok(())
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
        set_traversable(&path)?;

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
