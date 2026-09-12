//! Transitive dependency resolution.
//!
//! Pure given a [`RegistryClient`]: this module fetches metadata and returns a
//! graph, writing nothing to disk. That is what lets the tree-shape tests run
//! with no filesystem at all, and it is why the lockfile can be a straight
//! serialization of the result.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::integrity::{Integrity, IntegrityError};
use crate::range::{Range, RangeError};
use crate::registry::{Packument, RegistryClient, RegistryError};

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Range(#[from] RangeError),
    #[error("no version of `{name}` satisfies `{range}` (available: {})", available.join(", "))]
    Unsatisfiable {
        name: String,
        range: String,
        available: Vec<String>,
    },
    #[error(
        "`{name}` has a `{tag}` tag pointing at version `{version}`, which it does not publish"
    )]
    DanglingTag {
        name: String,
        tag: String,
        version: String,
    },
    #[error("`{spec}` is neither a valid version range nor a dist-tag of `{name}` (tags: {})", tags.join(", "))]
    UnresolvableSpec {
        name: String,
        spec: String,
        tags: Vec<String>,
    },
    #[error("`{name}@{version}` has no usable integrity hash")]
    Integrity {
        name: String,
        version: String,
        #[source]
        source: IntegrityError,
    },
}

/// A node in the resolved graph: one concrete version of one package.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageId {
    pub name: String,
    pub version: String,
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub id: PackageId,
    /// The tarball URL, carried through so the lockfile records it.
    pub resolved: String,
    pub integrity: Integrity,
    /// What this package calls a dependency -> the node it resolved to.
    pub dependencies: BTreeMap<String, PackageId>,
}

/// A workspace-relative directory that declares dependencies. `.` is the
/// workspace root.
///
/// Deliberately not a package identifier. An importer is a *consumer* —
/// something that declares dependencies and receives a `node_modules` — and is
/// identified by where it sits, because that is where its `node_modules` goes.
/// A `PackageId` identifies a *resolved artifact*. A workspace member is both,
/// but the two roles need different identities: rename the package and it is
/// still the same importer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImporterPath(String);

#[derive(Debug, Error)]
pub enum ImporterPathError {
    #[error("importer path `{0}` is absolute; importers are workspace-relative")]
    Absolute(String),
    #[error("importer path `{0}` escapes the workspace root")]
    EscapesRoot(String),
    #[error("importer path cannot be empty; the workspace root is `.`")]
    Empty,
}

impl ImporterPath {
    /// The workspace root, and the only importer a single-project repo has.
    pub fn root() -> Self {
        ImporterPath(".".to_string())
    }

    /// Validate a workspace-relative directory.
    ///
    /// Importer paths arrive from two untrusted places — a lockfile that may
    /// have been hand-edited, and `workspaces` globs in a manifest — so this
    /// refuses anything that could name a directory outside the workspace.
    /// That is the same class of check `archive` applies to tar entries, and
    /// for the same reason.
    pub fn new(raw: impl Into<String>) -> Result<Self, ImporterPathError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(ImporterPathError::Empty);
        }
        if raw == "." {
            return Ok(ImporterPath::root());
        }

        let path = Path::new(&raw);
        if path.is_absolute() {
            return Err(ImporterPathError::Absolute(raw));
        }

        // Rebuilt from its `Normal` components rather than stored verbatim.
        // Refusing an escape is only half the job: `packages/ui`,
        // `./packages/ui`, `packages//ui` and `packages/ui/` all name one
        // directory, and keeping them as distinct keys would give that one
        // directory several importers — and, once linking exists, several
        // `node_modules`. This mirrors `archive::strip_prefix_component`,
        // which rebuilds for the same reason.
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => normalized.push(part),
                Component::CurDir => {}
                _ => return Err(ImporterPathError::EscapesRoot(raw)),
            }
        }

        if normalized.as_os_str().is_empty() {
            // Something like `./` or `.` that normalized away.
            return Ok(ImporterPath::root());
        }

        Ok(ImporterPath(normalized.to_string_lossy().into_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == "."
    }

    /// How many directories deep this importer sits below the workspace root.
    ///
    /// Used to decide whether a relative `link:` target stays inside the
    /// workspace: a `..` may not climb past the root. The linker does not use
    /// it — it derives each climb from where the link and its target diverge,
    /// so that one calculation covers importer, intra-store and local links
    /// alike rather than three depth rules kept in agreement.
    pub fn depth(&self) -> usize {
        if self.is_root() {
            0
        } else {
            // Normalized at construction, so every component is a plain name.
            self.0.split('/').count()
        }
    }
}

impl std::fmt::Display for ImporterPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a declared dependency actually came from.
///
/// An enum rather than a version string that sometimes starts with `link:`, so
/// the compiler makes every consumer say which case it handles. The `link:`
/// spelling exists only at the serialization boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Fetched from the registry. Has a node in `ResolvedGraph::packages`.
    Registry(PackageId),
    /// A workspace member, linked in place. No tarball and no integrity hash,
    /// because there is nothing to verify — the bytes are in the repo. The
    /// path is relative to the importer that declared it.
    Local(PathBuf),
}

/// One dependency as an importer declared it, and what it resolved to.
///
/// The pair does the work of two mechanisms. `specifier` is what the manifest
/// asked for, which is what makes staleness detectable. `resolution` is what
/// that became, which lets linking read the answer instead of re-deriving it.
/// It also records an alias faithfully — `execa` declared as
/// `npm:safe-execa@0.3.0` keeps the local name as its key.
#[derive(Debug, Clone)]
pub struct Dependency {
    pub specifier: String,
    pub resolution: Resolution,
}

/// One project in the workspace and the dependencies it declares.
#[derive(Debug, Clone, Default)]
pub struct Importer {
    pub dependencies: BTreeMap<String, Dependency>,
}

#[derive(Debug, Clone)]
pub struct ResolvedGraph {
    /// Every project in the workspace, keyed by directory. A single-project
    /// repo has exactly one, keyed `.`, and takes the same path as any other.
    pub importers: BTreeMap<ImporterPath, Importer>,
    /// `BTreeMap`, so iteration order is structural rather than incidental.
    /// Two machines resolving the same tree must serialize identically.
    pub packages: BTreeMap<PackageId, ResolvedPackage>,
}

/// One edge waiting to be resolved: who asked, for what name, at what range.
/// `dependent` is `None` for the root project's own dependencies.
struct Pending {
    dependent: Option<PackageId>,
    name: String,
    range: String,
}

/// Walk the dependency graph from the root's declared ranges.
pub fn resolve(
    registry: &dyn RegistryClient,
    roots: &BTreeMap<String, String>,
) -> Result<ResolvedGraph, ResolveError> {
    let mut packages: BTreeMap<PackageId, ResolvedPackage> = BTreeMap::new();
    let mut root_importer = Importer::default();

    // One request per package, however many dependents ask for it.
    let mut packuments: HashMap<String, Packument> = HashMap::new();
    // Identical (name, range) pairs select once.
    let mut selections: HashMap<(String, String), PackageId> = HashMap::new();

    let mut work: VecDeque<Pending> = roots
        .iter()
        .map(|(name, range)| Pending {
            dependent: None,
            name: name.clone(),
            range: range.clone(),
        })
        .collect();

    while let Some(Pending {
        dependent,
        name,
        range,
    }) = work.pop_front()
    {
        let id = select(registry, &mut packuments, &mut selections, &name, &range)?;

        // Record the edge on whoever asked for it. A `None` dependent is an
        // importer's own dependency, which is recorded against the importer
        // rather than against a package.
        match dependent {
            Some(parent) => {
                if let Some(package) = packages.get_mut(&parent) {
                    package.dependencies.insert(name.clone(), id.clone());
                }
            }
            None => {
                root_importer.dependencies.insert(
                    name.clone(),
                    Dependency {
                        specifier: range.clone(),
                        resolution: Resolution::Registry(id.clone()),
                    },
                );
            }
        }

        // Recursion is gated on node novelty, not on path. That is what makes
        // a cycle terminate: the second visit finds the node present, records
        // the edge above, and stops here without needing a visited-path stack.
        if packages.contains_key(&id) {
            continue;
        }

        let packument = packuments
            .get(&id.name)
            .expect("select fetched this packument");
        // Both paths in `select` check membership before returning: the range
        // path picks from `versions_sorted`, and the tag path rejects a
        // dangling target. Neither can hand back a version that is absent.
        let metadata = packument
            .versions
            .get(&id.version)
            .expect("select verified this version is present");

        let integrity = metadata
            .dist
            .integrity()
            .map_err(|source| ResolveError::Integrity {
                name: id.name.clone(),
                version: id.version.clone(),
                source,
            })?;

        // `dependencies` only. A dependency's `devDependencies` must never be
        // followed — doing so pulls in most of the registry — which is why
        // `VersionMetadata` has no field for them to be read from.
        for (dep_name, dep_range) in &metadata.dependencies {
            work.push_back(Pending {
                dependent: Some(id.clone()),
                name: dep_name.clone(),
                range: dep_range.clone(),
            });
        }

        packages.insert(
            id.clone(),
            ResolvedPackage {
                id,
                resolved: metadata.dist.tarball.clone(),
                integrity,
                dependencies: BTreeMap::new(),
            },
        );
    }

    // One importer, keyed `.`. A workspace of many is the same shape with more
    // entries, which is why there is no single-project branch here to keep
    // working when Task 7 widens the input.
    let mut importers = BTreeMap::new();
    importers.insert(ImporterPath::root(), root_importer);

    Ok(ResolvedGraph {
        importers,
        packages,
    })
}

/// Choose the concrete version satisfying one `(name, range)` pair, fetching
/// and caching the packument as needed.
fn select(
    registry: &dyn RegistryClient,
    packuments: &mut HashMap<String, Packument>,
    selections: &mut HashMap<(String, String), PackageId>,
    name: &str,
    range: &str,
) -> Result<PackageId, ResolveError> {
    let key = (name.to_string(), range.to_string());
    if let Some(id) = selections.get(&key) {
        return Ok(id.clone());
    }

    if !packuments.contains_key(name) {
        packuments.insert(name.to_string(), registry.packument(name)?);
    }
    let packument = &packuments[name];

    // Range syntax is tried first, and a dist-tag is only the fallback for a
    // spec that is not a range at all. npm resolves in this order for a
    // reason: were tags consulted first, a registry could publish a tag named
    // `^1.0.0` and silently override what that range means.
    let version = match Range::parse(range) {
        Ok(parsed) => {
            let available = packument.versions_sorted();
            match parsed.max_satisfying(&available) {
                Some(chosen) => chosen.as_str().to_string(),
                None => {
                    return Err(ResolveError::Unsatisfiable {
                        name: name.to_string(),
                        range: range.to_string(),
                        available: available.iter().map(|v| v.as_str().to_string()).collect(),
                    });
                }
            }
        }
        Err(_) => match packument.resolve_tag(range) {
            Some(tagged) => {
                // A tag is a pointer the registry maintains, and it can dangle:
                // unpublishing a version leaves the tag behind. Trusting it
                // blindly would panic on the lookup further down.
                if !packument.versions.contains_key(tagged) {
                    return Err(ResolveError::DanglingTag {
                        name: name.to_string(),
                        tag: range.to_string(),
                        version: tagged.to_string(),
                    });
                }
                tagged.to_string()
            }
            None => {
                return Err(ResolveError::UnresolvableSpec {
                    name: name.to_string(),
                    spec: range.to_string(),
                    tags: packument.dist_tags.keys().cloned().collect(),
                });
            }
        },
    };

    let id = PackageId {
        name: name.to_string(),
        version,
    };
    selections.insert(key, id.clone());
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_importer_is_dot_and_has_no_depth() {
        let root = ImporterPath::root();
        assert_eq!(root.as_str(), ".");
        assert!(root.is_root());
        assert_eq!(root.depth(), 0);
    }

    #[test]
    fn depth_counts_directories_below_the_root() {
        // What the linker uses to decide how far a symlink must climb to reach
        // the workspace root's virtual store.
        assert_eq!(ImporterPath::new("packages").unwrap().depth(), 1);
        assert_eq!(ImporterPath::new("packages/ui").unwrap().depth(), 2);
        assert_eq!(ImporterPath::new("apps/web/inner").unwrap().depth(), 3);
    }

    #[test]
    fn a_leading_current_directory_does_not_count_as_depth() {
        assert_eq!(ImporterPath::new("./packages/ui").unwrap().depth(), 2);
    }

    #[test]
    fn paths_escaping_the_workspace_are_refused() {
        for raw in ["../escape", "packages/../../escape", ".."] {
            assert!(
                matches!(
                    ImporterPath::new(raw),
                    Err(ImporterPathError::EscapesRoot(_))
                ),
                "{raw} should be refused"
            );
        }
    }

    #[test]
    fn absolute_paths_are_refused() {
        assert!(matches!(
            ImporterPath::new("/etc"),
            Err(ImporterPathError::Absolute(_))
        ));
    }

    #[test]
    fn an_empty_path_is_refused_because_the_root_is_dot() {
        assert!(matches!(
            ImporterPath::new(""),
            Err(ImporterPathError::Empty)
        ));
    }

    #[test]
    fn importers_sort_with_the_root_first() {
        // Serialization order is iteration order, and a lockfile reads better
        // with the root at the top.
        let mut paths = [
            ImporterPath::new("packages/ui").unwrap(),
            ImporterPath::root(),
            ImporterPath::new("apps/web").unwrap(),
        ];
        paths.sort();
        let order: Vec<&str> = paths.iter().map(|p| p.as_str()).collect();
        assert_eq!(order, [".", "apps/web", "packages/ui"]);
    }
}
