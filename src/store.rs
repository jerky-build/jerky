use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::integrity::Integrity;
use crate::staging::StagingDir;

/// Bumped when the on-disk store format changes. Package-level content
/// addressing is `v1`; a future file-level CAS would write `v2` and leave
/// old stores ignorable rather than corrupt. Both halves of the layout — this
/// version and the key below — are named only in this module, so that bump is
/// a change here rather than a hunt through call sites.
const STORE_VERSION: &str = "v1";

/// The directory name an integrity value addresses.
///
/// Lowercase hex rather than base64: macOS filesystems are case-insensitive by
/// default, so base64 keys can collide. The algorithm prefix keeps sha1 and
/// sha512 entries in separate namespaces.
///
/// Private on purpose. The store takes an `Integrity` and derives this itself,
/// so no caller can spell an entry's name — or spell one that disagrees with
/// what `commit` wrote.
fn entry_key(integrity: &Integrity) -> String {
    use std::fmt::Write as _;

    let mut key = format!("{}-", integrity.algo.name());
    for byte in &integrity.digest {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

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
/// Every operation is addressed by an [`Integrity`] — the hash of the bytes we
/// already verified — and the store derives the directory name from it. A
/// package version's identity is therefore its content, and two projects
/// wanting the same package share one directory.
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

    /// Where the entry for these bytes lives, whether or not it is present.
    pub fn entry_path(&self, integrity: &Integrity) -> PathBuf {
        self.versioned_root().join(entry_key(integrity))
    }

    pub fn contains(&self, integrity: &Integrity) -> bool {
        self.entry_path(integrity).is_dir()
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
    pub fn commit<F, E>(&self, integrity: &Integrity, populate: F) -> Result<PathBuf, StoreError>
    where
        F: FnOnce(&Path) -> Result<(), E>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let target = self.entry_path(integrity);
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
            key: entry_key(integrity),
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
                        key: entry_key(integrity),
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
    use crate::integrity::Algo;
    use tempfile::TempDir;

    #[derive(Debug, thiserror::Error)]
    #[error("populate failed on purpose")]
    struct Boom;

    /// A digest of the right length for its algorithm. The store never hashes
    /// anything, so these need only be distinct, not self-consistent.
    fn integrity(algo: Algo, fill: u8) -> Integrity {
        let len = match algo {
            Algo::Sha512 => 64,
            Algo::Sha1 => 20,
        };
        Integrity {
            algo,
            digest: vec![fill; len],
        }
    }

    fn abc() -> Integrity {
        integrity(Algo::Sha512, 0xab)
    }

    fn write_one_file(dir: &Path) -> Result<(), std::io::Error> {
        std::fs::write(dir.join("index.js"), "contents")
    }

    #[test]
    fn commit_creates_an_entry_and_contains_reports_it() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        assert!(!store.contains(&abc()));
        let path = store.commit(&abc(), write_one_file).unwrap();

        assert!(store.contains(&abc()));
        assert_eq!(path, store.entry_path(&abc()));
        assert_eq!(
            std::fs::read_to_string(path.join("index.js")).unwrap(),
            "contents"
        );
    }

    #[test]
    fn one_integrity_value_addresses_one_entry_across_every_operation() {
        // The caller never spells a key, so `contains`, `entry_path` and
        // `commit` agree only if they derive it the same way. An equal-valued
        // `Integrity` built separately must land on the same entry.
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        let committed = store.commit(&abc(), write_one_file).unwrap();

        let same_value = abc();
        assert!(store.contains(&same_value));
        assert_eq!(store.entry_path(&same_value), committed);
        assert_eq!(
            store.commit(&same_value, write_one_file).unwrap(),
            committed
        );
    }

    #[test]
    fn distinct_integrity_values_never_collide() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        let other = integrity(Algo::Sha512, 0xcd);
        let legacy = integrity(Algo::Sha1, 0xab);

        store.commit(&abc(), write_one_file).unwrap();

        assert!(!store.contains(&other));
        assert!(!store.contains(&legacy));
        assert_ne!(store.entry_path(&abc()), store.entry_path(&other));
        // A sha1 entry and a sha512 entry are separate even when their digests
        // share a prefix: the algorithm namespaces the key.
        assert_ne!(store.entry_path(&abc()), store.entry_path(&legacy));
    }

    #[test]
    fn entry_keys_are_lowercase_and_algo_prefixed() {
        // macOS filesystems are case-insensitive by default, so a base64 key
        // could address two digests with one directory. Lowercase hex cannot.
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        let key = store
            .entry_path(&abc())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        assert!(key.starts_with("sha512-"));
        assert_eq!(
            key,
            key.to_lowercase(),
            "store keys must be case-safe on macOS"
        );
        // "sha512-" plus 64 bytes rendered as two hex chars each.
        assert_eq!(key.len(), 7 + 128);
    }

    #[test]
    fn entries_live_under_a_versioned_directory() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        let entry = store.entry_path(&abc());

        assert_eq!(entry.parent().unwrap(), root.path().join("v1"));
        assert_eq!(store.entry_path_root(), root.path().join("v1"));
    }

    #[test]
    fn staging_happens_inside_the_store_root() {
        // rename(2) is atomic only within one filesystem. If staging escaped to
        // TMPDIR — often a separate tmpfs — commit would fail with EXDEV.
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        store
            .commit(&abc(), |staging| {
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

        let result = store.commit(&abc(), |_| Err(Boom));

        assert!(matches!(result, Err(StoreError::Populate { .. })));
        assert!(!store.contains(&abc()));
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
        store.commit(&abc(), write_one_file).unwrap();

        // The race loser: another process already committed this entry.
        let path = store
            .commit(&abc(), |dir| {
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

        let first = store.commit(&abc(), write_one_file).unwrap();
        let second = store.commit(&abc(), write_one_file).unwrap();

        assert_eq!(first, second);
    }
}
