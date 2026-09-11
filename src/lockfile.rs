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
use crate::resolver::{
    Dependency, Importer, ImporterPath, PackageId, Resolution, ResolvedGraph, ResolvedPackage,
};

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
    #[error("{path} records importer `{importer}`, which is not a usable workspace path")]
    BadImporter {
        path: PathBuf,
        importer: String,
        #[source]
        source: crate::resolver::ImporterPathError,
    },
    #[error(
        "{path} has importer `{importer}` depending on `{name}`, recorded as `{version}`, which it does not record"
    )]
    UnknownImporterDependency {
        path: PathBuf,
        importer: String,
        name: String,
        version: String,
    },
    #[error("{path} keys `{entry}` but records version `{declared}` inside it")]
    KeyVersionMismatch {
        path: PathBuf,
        entry: String,
        declared: String,
    },
    #[error("{path} has `{entry}` depending on `{dependency}`, which it does not record")]
    DanglingEdge {
        path: PathBuf,
        entry: String,
        dependency: String,
    },
    #[error("{path} records an unusable integrity hash for `{entry}`")]
    BadIntegrity {
        path: PathBuf,
        entry: String,
        #[source]
        source: crate::integrity::IntegrityError,
    },
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
    /// Every project in the workspace, keyed by directory, each recording what
    /// it asked for and what that resolved to. A single-project repo has one
    /// entry keyed `.`.
    ///
    /// Recording the specifier is what makes staleness detectable, and
    /// recording it per importer is what makes staleness *per importer*: one
    /// edited project no longer invalidates everything the rest resolved.
    importers: BTreeMap<String, OnDiskImporter>,
    /// Keyed `name@version`, flat rather than nested, and shared by every
    /// importer. A nested tree mirroring the graph would reindent every
    /// descendant when something deep changes; flat means adding a dependency
    /// appends a block and touches nothing else.
    packages: BTreeMap<String, Entry>,
}

#[derive(Serialize, Deserialize, Default)]
struct OnDiskImporter {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    dependencies: BTreeMap<String, OnDiskDependency>,
}

#[derive(Serialize, Deserialize)]
struct OnDiskDependency {
    /// What the manifest asked for, verbatim.
    specifier: String,
    /// What it resolved to: a concrete version, or `link:<path>` for a
    /// workspace member. The prefix is the only place `Resolution`'s two cases
    /// are spelled rather than typed.
    version: String,
}

/// The marker distinguishing a linked workspace member from a registry version.
const LINK_PREFIX: &str = "link:";

/// Just enough of a lockfile to read its version, whatever else it holds.
#[derive(Deserialize)]
struct VersionProbe {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
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

    let importers = graph
        .importers
        .iter()
        .map(|(path, importer)| {
            (
                path.as_str().to_string(),
                OnDiskImporter {
                    dependencies: importer
                        .dependencies
                        .iter()
                        .map(|(name, dependency)| {
                            (
                                name.clone(),
                                OnDiskDependency {
                                    specifier: dependency.specifier.clone(),
                                    version: match &dependency.resolution {
                                        Resolution::Registry(id) => id.version.clone(),
                                        Resolution::Local(target) => {
                                            format!("{LINK_PREFIX}{}", target.display())
                                        }
                                    },
                                },
                            )
                        })
                        .collect(),
                },
            )
        })
        .collect();

    let on_disk = OnDisk {
        lockfile_version: LOCKFILE_VERSION,
        importers,
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

    // The version is read before the file is parsed as the *current* schema,
    // and the order matters. A future format may be perfectly valid JSON while
    // naming entirely different fields; deserializing first would report it as
    // malformed, which is both wrong and unhelpful — the user needs to be told
    // to upgrade jerky, not to go hunting for a syntax error.
    let probe: VersionProbe =
        serde_json::from_str(&raw).map_err(|source| LockfileError::Malformed {
            path: path.clone(),
            source,
        })?;

    if probe.lockfile_version != LOCKFILE_VERSION {
        return Err(LockfileError::UnsupportedVersion {
            path,
            found: probe.lockfile_version,
            supported: LOCKFILE_VERSION,
        });
    }

    let on_disk: OnDisk =
        serde_json::from_str(&raw).map_err(|source| LockfileError::Malformed {
            path: path.clone(),
            source,
        })?;

    let mut packages = BTreeMap::new();
    for (key, entry) in on_disk.packages {
        let (name, keyed_version) = split_key(&key).ok_or_else(|| LockfileError::BadKey {
            path: path.clone(),
            entry: key.clone(),
        })?;

        // The key and the entry state the version twice, so they can disagree.
        // Trusting one and ignoring the other lets two keys collapse into one
        // package — silently dropping a dependency while keeping the wrong
        // tarball URL — which is exactly what a hand-edited lockfile produces.
        if keyed_version != entry.version {
            return Err(LockfileError::KeyVersionMismatch {
                path,
                entry: key.clone(),
                declared: entry.version.clone(),
            });
        }

        let integrity =
            Integrity::parse(&entry.integrity).map_err(|source| LockfileError::BadIntegrity {
                path: path.clone(),
                entry: key.clone(),
                source,
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

    // Edges are checked only once every node is known, since a dependency may
    // be recorded after its dependent. An edge pointing at nothing means the
    // graph cannot be installed, and saying so beats discovering it halfway
    // through linking.
    for package in packages.values() {
        for dependency in package.dependencies.values() {
            if !packages.contains_key(dependency) {
                return Err(LockfileError::DanglingEdge {
                    path,
                    entry: package.id.to_string(),
                    dependency: dependency.to_string(),
                });
            }
        }
    }

    let mut importers = BTreeMap::new();
    for (raw, on_disk_importer) in on_disk.importers {
        // Importer keys come from a file that may have been hand-edited, so a
        // key naming a directory outside the workspace is refused rather than
        // resolved — the same reasoning `archive` applies to tar entries.
        let importer_path =
            ImporterPath::new(raw.clone()).map_err(|source| LockfileError::BadImporter {
                path: path.clone(),
                importer: raw.clone(),
                source,
            })?;

        let mut dependencies = BTreeMap::new();
        for (name, dependency) in on_disk_importer.dependencies {
            let resolution = match dependency.version.strip_prefix(LINK_PREFIX) {
                Some(target) => Resolution::Local(PathBuf::from(target)),
                None => {
                    // The name a dependency is declared under need not be the
                    // package it resolves to — `execa` declared as
                    // `npm:safe-execa@0.3.0` is a real example — so the target
                    // is looked up by the version recorded against it, and
                    // having no such package is an error rather than a
                    // fabricated node.
                    let id = packages
                        .keys()
                        .find(|id| id.name == name && id.version == dependency.version)
                        .cloned()
                        .ok_or_else(|| LockfileError::UnknownImporterDependency {
                            path: path.clone(),
                            importer: raw.clone(),
                            name: name.clone(),
                            version: dependency.version.clone(),
                        })?;
                    Resolution::Registry(id)
                }
            };

            dependencies.insert(
                name,
                Dependency {
                    specifier: dependency.specifier,
                    resolution,
                },
            );
        }

        importers.insert(importer_path, Importer { dependencies });
    }

    Ok(Some(ResolvedGraph {
        importers,
        packages,
    }))
}
