//! `jerky-lock.json`: the resolved graph, written down.
//!
//! The on-disk shape is a separate type from [`ResolvedGraph`] on purpose. The
//! graph is whatever the resolver finds convenient; the lockfile is what is
//! stable to commit. Conflating them would make every internal refactor a
//! format change, and this is a file real projects commit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::integrity::Integrity;
use crate::resolver::{PackageId, ResolvedGraph, ResolvedPackage};

pub const LOCKFILE_NAME: &str = "jerky-lock.json";

/// Bumped when the on-disk shape changes.
///
/// Present from the first release because a committed format without one has
/// no migration path: there is no way to tell an old file from a corrupt one.
pub const LOCKFILE_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum LockfileError {
    #[error("{path} is not valid JSON")]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "{path} uses lockfile version {found}, but this jerky understands {supported}. Upgrade jerky."
    )]
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("{path} records `{entry}`, which is not a valid name@version key")]
    BadKey { path: PathBuf, entry: String },
    #[error("{path} records an unusable integrity hash for `{entry}`")]
    BadIntegrity { path: PathBuf, entry: String },
    #[error("failed to access {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Serialize, Deserialize)]
struct OnDisk {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
    /// The ranges the manifest declared, so staleness is detectable. Without
    /// it there is no way to tell a current lockfile from one written before
    /// someone edited `package.json`.
    root: BTreeMap<String, String>,
    /// Keyed `name@version`, flat rather than nested. A nested tree mirroring
    /// the graph would reindent every descendant when something deep changes;
    /// flat means adding a dependency appends a block and touches nothing else.
    packages: BTreeMap<String, Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    version: String,
    resolved: String,
    integrity: String,
    /// Edge values are the concrete version chosen, not the range asked for:
    /// the lockfile records what a resolution decided, not what it was given.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    dependencies: BTreeMap<String, String>,
}

/// Split a `name@version` key.
///
/// Splits on the *last* `@` so that `@types/node@20.1.0` yields the name
/// `@types/node`. Scoped installs are spec 3, but the key format has to
/// survive them or the lockfile needs a migration the day they land — the same
/// reasoning spec 1 applied to `parse_package_spec`.
fn split_key(key: &str) -> Option<(&str, &str)> {
    key.rsplit_once('@')
        .filter(|(name, version)| !name.is_empty() && !version.is_empty())
}

pub fn save(graph: &ResolvedGraph, project_dir: &Path) -> Result<(), LockfileError> {
    let packages = graph
        .packages
        .values()
        .map(|package| {
            (
                package.id.to_string(),
                Entry {
                    version: package.id.version.clone(),
                    resolved: package.resolved.clone(),
                    integrity: package.integrity.to_ssri(),
                    dependencies: package
                        .dependencies
                        .iter()
                        .map(|(name, id)| (name.clone(), id.version.clone()))
                        .collect(),
                },
            )
        })
        .collect();

    let on_disk = OnDisk {
        lockfile_version: LOCKFILE_VERSION,
        root: graph.root.clone(),
        packages,
    };

    let path = project_dir.join(LOCKFILE_NAME);
    let mut text =
        serde_json::to_string_pretty(&on_disk).expect("a lockfile is always serializable");
    text.push('\n');

    std::fs::write(&path, text).map_err(|source| LockfileError::Io { path, source })
}

/// Read the lockfile, if there is one.
///
/// `Ok(None)` means no lockfile exists, which is normal — the first install
/// has none. Every other failure is an error rather than a silent fallback to
/// re-resolving: a lockfile jerky cannot understand is a signal, not noise.
pub fn load(project_dir: &Path) -> Result<Option<ResolvedGraph>, LockfileError> {
    let path = project_dir.join(LOCKFILE_NAME);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(LockfileError::Io { path, source }),
    };

    let on_disk: OnDisk =
        serde_json::from_str(&raw).map_err(|source| LockfileError::Malformed {
            path: path.clone(),
            source,
        })?;

    // Checked before anything else is trusted: a file from a future format may
    // parse as JSON while meaning something entirely different.
    if on_disk.lockfile_version != LOCKFILE_VERSION {
        return Err(LockfileError::UnsupportedVersion {
            path,
            found: on_disk.lockfile_version,
            supported: LOCKFILE_VERSION,
        });
    }

    let mut packages = BTreeMap::new();
    for (key, entry) in on_disk.packages {
        let (name, _) = split_key(&key).ok_or_else(|| LockfileError::BadKey {
            path: path.clone(),
            entry: key.clone(),
        })?;

        let integrity =
            Integrity::parse(&entry.integrity).map_err(|_| LockfileError::BadIntegrity {
                path: path.clone(),
                entry: key.clone(),
            })?;

        let id = PackageId {
            name: name.to_string(),
            version: entry.version.clone(),
        };

        let dependencies = entry
            .dependencies
            .iter()
            .map(|(dep_name, dep_version)| {
                (
                    dep_name.clone(),
                    PackageId {
                        name: dep_name.clone(),
                        version: dep_version.clone(),
                    },
                )
            })
            .collect();

        packages.insert(
            id.clone(),
            ResolvedPackage {
                id,
                resolved: entry.resolved,
                integrity,
                dependencies,
            },
        );
    }

    Ok(Some(ResolvedGraph {
        root: on_disk.root,
        packages,
    }))
}
