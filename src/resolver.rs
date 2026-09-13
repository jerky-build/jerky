//! Transitive dependency resolution.
//!
//! Pure given a [`RegistryClient`]: this module fetches metadata and returns a
//! graph, writing nothing to disk. That is what lets the tree-shape tests run
//! with no filesystem at all, and it is why the lockfile can be a straight
//! serialization of the result.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::integrity::{Integrity, IntegrityError};
use crate::range::{Range, RangeError};
use crate::registry::{Packument, RegistryClient, RegistryError};

/// The protocol marking a dependency as a workspace member rather than a
/// registry package. `workspace:*` and `workspace:^1.0.0` both select the
/// member; what follows the colon is a range against the member's own version,
/// which matters only once a member is published.
const WORKSPACE_PROTOCOL: &str = "workspace:";

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
    #[error("`{specifier}` names `{name}`, which is not a workspace member (members: {})", members.join(", "))]
    NoSuchMember {
        name: String,
        specifier: String,
        members: Vec<String>,
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

impl Importer {
    /// Does this record exactly what a manifest declares?
    ///
    /// Both directions matter: a dependency added to the manifest is missing
    /// from the record, and one deleted from it lingers there. Either way what
    /// was recorded no longer describes what was asked for, which is the whole
    /// question a lockfile's specifiers exist to answer.
    pub fn matches(&self, declared: &BTreeMap<String, String>) -> bool {
        self.dependencies.len() == declared.len()
            && declared.iter().all(|(name, specifier)| {
                self.dependencies
                    .get(name)
                    .is_some_and(|dependency| &dependency.specifier == specifier)
            })
    }
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

/// Who asked for an edge.
///
/// An enum rather than an `Option<PackageId>` standing in for "the root", now
/// that there is more than one importer to be: `None` could only ever mean one
/// project, and the compiler would not have asked about the rest.
enum Dependent {
    /// A project declaring its own dependency, identified by where it sits.
    Importer(ImporterPath),
    /// A package declaring one of its own.
    Package(PackageId),
}

impl ResolvedGraph {
    /// The same graph with packages no importer can reach dropped.
    ///
    /// A full resolution only ever produces the reachable set, so this keeps a
    /// property the lockfile already had rather than adding one. Reuse is what
    /// would break it: merging a reused importer's packages with a re-resolved
    /// one's accumulates entries nothing references, and nothing fails when it
    /// happens — the file just grows, and every diff touching it gets noisier,
    /// which costs the flat format the minimal-diff property it was flattened
    /// to get.
    ///
    /// Spec 2 §12 deferred pruning until an `uninstall` command existed to
    /// trigger it, and §2 now settles it the other way: a dependency deleted
    /// from a `package.json` by hand loses its subtree on the next install,
    /// and an `uninstall` command will need no pruning of its own.
    pub fn reachable(self) -> Self {
        let mut reachable: BTreeSet<PackageId> = BTreeSet::new();
        let mut queue: VecDeque<PackageId> = self
            .importers
            .values()
            .flat_map(|importer| importer.dependencies.values())
            .filter_map(|dependency| match &dependency.resolution {
                Resolution::Registry(id) => Some(id.clone()),
                // A local dependency has no node in `packages`; it is the repo.
                Resolution::Local(_) => None,
            })
            .collect();

        while let Some(id) = queue.pop_front() {
            if !reachable.insert(id.clone()) {
                continue;
            }
            if let Some(package) = self.packages.get(&id) {
                queue.extend(package.dependencies.values().cloned());
            }
        }

        ResolvedGraph {
            importers: self.importers,
            packages: self
                .packages
                .into_iter()
                .filter(|(id, _)| reachable.contains(id))
                .collect(),
        }
    }
}

/// One edge waiting to be resolved: who asked, for what name, at what range.
struct Pending {
    dependent: Dependent,
    name: String,
    range: String,
}

/// Walk the dependency graph from every importer's declared ranges.
///
/// One walk covers the whole workspace rather than one per project, so two
/// importers asking for the same range select once and share the request. They
/// are deliberately not forced into agreement: importers wanting incompatible
/// versions each get their own node, exactly as two packages within one tree
/// do.
pub fn resolve(
    registry: &dyn RegistryClient,
    roots: &BTreeMap<ImporterPath, BTreeMap<String, String>>,
    members: &BTreeMap<String, ImporterPath>,
) -> Result<ResolvedGraph, ResolveError> {
    let mut packages: BTreeMap<PackageId, ResolvedPackage> = BTreeMap::new();
    // Seeded from the input rather than filled in as edges arrive, so an
    // importer that declares nothing still appears in the graph — it has a
    // `node_modules` to own, and the lockfile records it as resolved.
    let mut importers: BTreeMap<ImporterPath, Importer> = roots
        .keys()
        .map(|importer| (importer.clone(), Importer::default()))
        .collect();

    // One request per package, however many dependents ask for it.
    let mut packuments: HashMap<String, Packument> = HashMap::new();
    // Identical (name, range) pairs select once.
    let mut selections: HashMap<(String, String), PackageId> = HashMap::new();

    // Local dependencies are settled before the walk starts rather than
    // inside it: there is no tarball, no integrity hash and no version to
    // select, so a `workspace:` specifier has nothing the walk could do with
    // it. Only an importer may declare one — a registry package's
    // dependencies are whatever it published, and `VersionMetadata` has no
    // way to name a directory in this repo.
    let mut work: VecDeque<Pending> = VecDeque::new();
    for (importer, declared) in roots {
        for (name, range) in declared {
            if !range.starts_with(WORKSPACE_PROTOCOL) {
                work.push_back(Pending {
                    dependent: Dependent::Importer(importer.clone()),
                    name: name.clone(),
                    range: range.clone(),
                });
                continue;
            }

            let member = members
                .get(name)
                .ok_or_else(|| ResolveError::NoSuchMember {
                    name: name.clone(),
                    specifier: range.clone(),
                    members: members.keys().cloned().collect(),
                })?;

            importers
                .entry(importer.clone())
                .or_default()
                .dependencies
                .insert(
                    name.clone(),
                    Dependency {
                        specifier: range.clone(),
                        resolution: Resolution::Local(local_path(importer, member)),
                    },
                );
        }
    }

    while let Some(Pending {
        dependent,
        name,
        range,
    }) = work.pop_front()
    {
        let id = select(registry, &mut packuments, &mut selections, &name, &range)?;

        // Record the edge on whoever asked for it. An importer's own
        // dependency is recorded against the importer rather than against a
        // package, because that is what its `node_modules` is built from.
        match dependent {
            Dependent::Package(parent) => {
                if let Some(package) = packages.get_mut(&parent) {
                    package.dependencies.insert(name.clone(), id.clone());
                }
            }
            Dependent::Importer(importer) => {
                importers.entry(importer).or_default().dependencies.insert(
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
                dependent: Dependent::Package(id.clone()),
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

    Ok(ResolvedGraph {
        importers,
        packages,
    })
}

/// Where `member` sits, as seen from `importer`.
///
/// Climbing is driven by [`ImporterPath::depth`]: both paths are
/// workspace-relative, so reaching the root takes exactly as many steps as the
/// declaring importer is deep, and the member's own path descends from there.
/// The result is the lockfile's `link:` value, relative to the importer's own
/// directory. The symlink the linker writes is deliberately *not* this string:
/// a link lives inside the importer's `node_modules`, one level deeper, so
/// `linker::relative_path` derives its own target from absolute paths rather
/// than adding a climb to this one. `a_lockfile_target_is_one_climb_short_of_
/// the_symlink` pins the two together.
fn local_path(importer: &ImporterPath, member: &ImporterPath) -> PathBuf {
    let mut path = PathBuf::new();
    for _ in 0..importer.depth() {
        path.push(Component::ParentDir);
    }
    if !member.is_root() {
        path.push(member.as_str());
    }
    if path.as_os_str().is_empty() {
        // An importer depending on itself, or the root on the root. `.` is a
        // path; the empty string is not.
        path.push(Component::CurDir);
    }
    path
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
