//! Transitive dependency resolution.
//!
//! Pure given a [`RegistryClient`]: this module fetches metadata and returns a
//! graph, writing nothing to disk. That is what lets the tree-shape tests run
//! with no filesystem at all, and it is why the lockfile can be a straight
//! serialization of the result.

use std::collections::{BTreeMap, HashMap, VecDeque};

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

#[derive(Debug, Clone)]
pub struct ResolvedGraph {
    /// The ranges the root manifest declared, recorded verbatim so a lockfile
    /// written from this graph can later be told apart from a stale one.
    pub root: BTreeMap<String, String>,
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

        // Record the edge on whoever asked for it. The root's edges are not
        // recorded here — they live in `root` — so `None` is simply skipped.
        if let Some(parent) = dependent
            && let Some(package) = packages.get_mut(&parent)
        {
            package.dependencies.insert(name.clone(), id.clone());
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
        let metadata = packument
            .versions
            .get(&id.version)
            .expect("select chose this version from this packument");

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

    Ok(ResolvedGraph {
        root: roots.clone(),
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

    // A dist-tag is not a range, so the tag table is consulted first. This is
    // the same mechanism spec 1's single-version path uses for `latest`.
    let version = match packument.resolve_tag(range) {
        Some(tagged) => tagged.to_string(),
        None => {
            let parsed = Range::parse(range)?;
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
    };

    let id = PackageId {
        name: name.to_string(),
        version,
    };
    selections.insert(key, id.clone());
    Ok(id)
}
