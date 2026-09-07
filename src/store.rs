use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::staging::StagingDir;

/// Bumped when the on-disk store format changes. Package-level content
/// addressing is `v1`; a future file-level CAS would write `v2` and leave
/// old stores ignorable rather than corrupt.
const STORE_VERSION: &str = "v1";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("failed to prepare the store at {path}")]
    StagingFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to commit {key} into the store")]
    CommitFailed {
        key: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to populate the store entry for {key}")]
    Populate {
        key: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// A content-addressed package store.
///
/// Keys are `Integrity::store_key()` values, so a package version's identity
/// is the hash of the bytes we already verified. Two projects wanting the same
/// package share one directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn versioned_root(&self) -> PathBuf {
        self.root.join(STORE_VERSION)
    }

    /// The directory holding all entries. Exposed so tests can assert the
    /// store is empty after a rejected install.
    pub fn entry_path_root(&self) -> PathBuf {
        self.versioned_root()
    }

    pub fn entry_path(&self, key: &str) -> PathBuf {
        self.versioned_root().join(key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entry_path(key).is_dir()
    }

    /// Populate a store entry atomically.
    ///
    /// `populate` writes into a staging directory; on success the directory is
    /// renamed into its final content-addressed location. Extracting straight
    /// to the final path would mean a killed process leaves a *partial*
    /// directory at exactly the path that means "present and verified", so
    /// every later install would hard-link a truncated package into a project.
    ///
    /// Returns the entry path. If the entry already exists — because this
    /// process committed it earlier or another process won a race — `populate`
    /// is never called and the existing entry is left untouched.
    pub fn commit<F, E>(&self, key: &str, populate: F) -> Result<PathBuf, StoreError>
    where
        F: FnOnce(&Path) -> Result<(), E>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let target = self.entry_path(key);
        if target.is_dir() {
            return Ok(target);
        }

        // The staging directory lives inside the store root so the rename below
        // never crosses a filesystem boundary.
        let mut staging = StagingDir::create_under(&self.versioned_root()).map_err(|err| {
            StoreError::StagingFailed {
                path: err.path,
                source: err.source,
            }
        })?;

        populate(staging.path()).map_err(|source| StoreError::Populate {
            key: key.to_string(),
            source: Box::new(source),
        })?;

        let staged = staging.keep();
        match std::fs::rename(&staged, &target) {
            Ok(()) => Ok(target),
            Err(source) => {
                // Renaming a directory onto a non-empty one fails rather than
                // clobbering, so losing the race is a success: the entry the
                // winner wrote is byte-identical, since the key is its hash.
                let _ = std::fs::remove_dir_all(&staged);
                if target.is_dir() {
                    Ok(target)
                } else {
                    Err(StoreError::CommitFailed {
                        key: key.to_string(),
                        source,
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[derive(Debug, thiserror::Error)]
    #[error("populate failed on purpose")]
    struct Boom;

    fn write_one_file(dir: &Path) -> Result<(), std::io::Error> {
        std::fs::write(dir.join("index.js"), "contents")
    }

    #[test]
    fn commit_creates_an_entry_and_contains_reports_it() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        assert!(!store.contains("sha512-abc"));
        let path = store.commit("sha512-abc", write_one_file).unwrap();

        assert!(store.contains("sha512-abc"));
        assert_eq!(path, store.entry_path("sha512-abc"));
        assert_eq!(
            std::fs::read_to_string(path.join("index.js")).unwrap(),
            "contents"
        );
    }

    #[test]
    fn entries_live_under_a_versioned_directory() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());
        assert_eq!(store.entry_path("k"), root.path().join("v1").join("k"));
    }

    #[test]
    fn staging_happens_inside_the_store_root() {
        // rename(2) is atomic only within one filesystem. If staging escaped to
        // TMPDIR — often a separate tmpfs — commit would fail with EXDEV.
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        store
            .commit("sha512-abc", |staging| {
                assert!(
                    staging.starts_with(root.path()),
                    "staging dir {} escaped the store root",
                    staging.display()
                );
                write_one_file(staging)
            })
            .unwrap();
    }

    #[test]
    fn a_failed_populate_leaves_the_store_empty() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        let result = store.commit("sha512-abc", |_| Err(Boom));

        assert!(matches!(result, Err(StoreError::Populate { .. })));
        assert!(!store.contains("sha512-abc"));
        // And no staging debris is left behind.
        let staging = root.path().join("v1").join(".staging");
        if staging.exists() {
            assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
        }
    }

    #[test]
    fn an_existing_entry_is_not_repopulated() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());
        store.commit("sha512-abc", write_one_file).unwrap();

        // The race loser: another process already committed this key.
        let path = store
            .commit("sha512-abc", |dir| {
                std::fs::write(dir.join("index.js"), "different")
            })
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(path.join("index.js")).unwrap(),
            "contents",
            "an existing entry must never be clobbered"
        );
    }

    #[test]
    fn commit_is_idempotent() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        let first = store.commit("sha512-abc", write_one_file).unwrap();
        let second = store.commit("sha512-abc", write_one_file).unwrap();

        assert_eq!(first, second);
    }
}
