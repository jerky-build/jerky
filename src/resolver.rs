//! Transitive dependency resolution.
//!
//! Pure given a [`RegistryClient`]: this module fetches metadata and returns a
//! graph, writing nothing to disk. That is what lets the tree-shape tests run
//! with no filesystem at all, and it is why the lockfile can be a straight
//! serialization of the result.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use thiserror::Error;

use crate::integrity::{Integrity, IntegrityError};
use crate::pool;
use crate::range::{Range, RangeError};
use crate::registry::{
    Freshness, MAX_CONCURRENT_FETCHES, Packument, RegistryClient, RegistryError,
};

/// The protocol marking a dependency as a workspace member rather than a
/// registry package. `workspace:*` and `workspace:^1.0.0` both select the
/// member; what follows the colon is a range against the member's own version,
/// which matters only once a member is published.
const WORKSPACE_PROTOCOL: &str = "workspace:";

/// The scheme that renames a package: `npm:<name>@<range>` asks for one
/// package under a different local name.
pub const ALIAS_PROTOCOL: &str = "npm:";

/// The package an `npm:` specifier names, and the range it asks of it — or
/// `None` for every specifier that is not an alias.
///
/// Public because the resolver is not the only place that has to understand
/// one: recording a request in a manifest has to keep the scheme, or the pin
/// it writes names a package no registry serves.
pub fn alias_target(specifier: &str) -> Option<(&str, &str)> {
    specifier.strip_prefix(ALIAS_PROTOCOL).map(split_target)
}

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
    #[error(
        "`{name}` is declared as `{specifier}`, and jerky does not understand `{scheme}:` specifiers"
    )]
    UnsupportedScheme {
        name: String,
        specifier: String,
        scheme: String,
    },
    #[error("`{name}` is declared as `{specifier}`, which names no package to alias")]
    MalformedAlias { name: String, specifier: String },
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

/// Which section of a manifest declared a dependency.
///
/// Only an importer ever has one. A registry package's dependencies are all
/// `Prod` by construction, because `VersionMetadata` has no field a dev
/// dependency could arrive through — which is where that correctness
/// requirement is enforced, rather than by anything here remembering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Prod,
    Dev,
}

/// What an importer declares: a specifier, and which section it came from.
///
/// The pair is the resolver's input and the unit staleness is measured in. A
/// specifier alone cannot tell `dependencies` from `devDependencies`, so a
/// dependency moved between the two at an unchanged specifier would read as no
/// change at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub specifier: String,
    pub kind: Kind,
}

/// One dependency as an importer declared it, and what it resolved to.
///
/// The pair does the work of two mechanisms. `specifier` is what the manifest
/// asked for, which is what makes staleness detectable. `resolution` is what
/// that became, which lets linking read the answer instead of re-deriving it.
/// It also records an alias faithfully — `execa` declared as
/// `npm:safe-execa@0.3.0` keeps the local name as its key.
///
/// `kind` is a field rather than a second map on [`Importer`], so a name can
/// only ever mean one thing and every lookup stays single-branch. The split
/// into two blocks happens at serialization, where the on-disk format wants
/// it; the graph has no use for it.
#[derive(Debug, Clone)]
pub struct Dependency {
    pub specifier: String,
    pub kind: Kind,
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
    ///
    /// The comparison is over `(specifier, kind)` pairs rather than specifiers
    /// alone. A dependency moved from `dependencies` to `devDependencies`
    /// without its specifier changing is a real edit, and one the lockfile
    /// records, so it has to make its importer stale — otherwise the file goes
    /// on describing a section the manifest has abandoned.
    pub fn matches(&self, declared: &BTreeMap<String, Declared>) -> bool {
        self.dependencies.len() == declared.len()
            && declared.iter().all(|(name, declared)| {
                self.dependencies.get(name).is_some_and(|dependency| {
                    dependency.specifier == declared.specifier && dependency.kind == declared.kind
                })
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
    /// A project declaring its own dependency, identified by where it sits,
    /// and by which of its sections asked. The kind rides along rather than
    /// steering anything: resolution is identical either way, and the value is
    /// only carried so the graph can record what the manifest said.
    Importer { path: ImporterPath, kind: Kind },
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
    /// The name this dependency is declared *under*, which is the key its edge
    /// is recorded at and the name its link takes. For an alias it is not the
    /// name of the package it resolves to.
    name: String,
    /// What the manifest or the packument said, verbatim.
    ///
    /// Recorded in the lockfile, so it is never normalised: `width-cjs`
    /// declared as `npm:string-width@^4.0.0` keeps the whole string, because
    /// staleness is measured against it and an edit that changed *which*
    /// package is aliased would otherwise read as no change at all.
    specifier: String,
    /// What to ask the registry, once the scheme has been read off. Equal to
    /// `(name, specifier)` for every specifier that is not an alias.
    asked: Asked,
}

/// The package a specifier asks for, and the range it asks of it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Asked {
    name: String,
    range: String,
}

impl Asked {
    /// Read a specifier for the package it names.
    ///
    /// Three cases, in the order they must be tried. `npm:` first, because the
    /// generic scheme check below would otherwise swallow the one scheme jerky
    /// understands. Then any other `<scheme>:`, refused by name — `file:`,
    /// `git:` and `github:` are all real, none are supported, and reporting
    /// one as a malformed version range says nothing a user can act on, which
    /// is exactly what jerky did with `npm:` before it understood one. And
    /// last the common case, a range or dist-tag for the package it is
    /// declared under.
    fn read(name: &str, specifier: &str) -> Result<Self, ResolveError> {
        if let Some((aliased, range)) = alias_target(specifier) {
            // A malformed alias is not an unsupported one, and must not be
            // reported as "jerky does not understand `npm:`" — the scheme is
            // the one thing here that is right.
            if !is_plausible_name(aliased) {
                return Err(ResolveError::MalformedAlias {
                    name: name.to_string(),
                    specifier: specifier.to_string(),
                });
            }
            return Ok(Self {
                name: aliased.to_string(),
                range: range.to_string(),
            });
        }

        if let Some(scheme) = scheme_of(specifier) {
            return Err(ResolveError::UnsupportedScheme {
                name: name.to_string(),
                specifier: specifier.to_string(),
                scheme: scheme.to_string(),
            });
        }

        Ok(Self {
            name: name.to_string(),
            range: specifier.to_string(),
        })
    }
}

/// Split `name@version` into its halves, or `None` if it is not that shape.
///
/// The *last* `@` is the separator, so `@types/node@20.1.0` yields the scoped
/// name and not an empty one — and `@scope/pkg` with no version yields `None`
/// rather than a name of `` and a version of `scope/pkg`. This is the inverse
/// of `Display for PackageId`, which is why it lives beside it; the lockfile
/// reads its keys and its recorded aliases with the same function, because
/// three spellings of one rule is three chances to disagree.
pub(crate) fn split_name_and_version(key: &str) -> Option<(&str, &str)> {
    key.rsplit_once('@')
        .filter(|(name, version)| !name.is_empty() && !version.is_empty())
}

/// Split an alias target into the package and the range asked of it.
///
/// A target with no version is `latest`, which is npm's answer and which the
/// dist-tag path already knows how to resolve.
///
/// Note this is *not* the rule `cli::parse_package_spec` applies. That one
/// splits on the **first** `@` past a scope, and the difference is what makes
/// `width-cjs@npm:string-width@^4.0.0` work at all: the command line takes the
/// first `@` to separate the local name from everything else, and this takes
/// the last to separate the aliased package from its range.
fn split_target(target: &str) -> (&str, &str) {
    split_name_and_version(target).unwrap_or((target, "latest"))
}

/// Is this a name the registry could conceivably answer to?
///
/// Only the shape a scope imposes, which is the part an alias can get wrong
/// without looking wrong: `npm:@1.0.0` splits into a "package" of `@1.0.0`
/// because the leading `@` reads as a scope, and would otherwise be sent to
/// the registry as a name. Everything else — a name the registry simply does
/// not have — is the registry's answer to give, not this function's.
fn is_plausible_name(name: &str) -> bool {
    match name.strip_prefix('@') {
        Some(scoped) => match scoped.split_once('/') {
            Some((scope, package)) => !scope.is_empty() && !package.is_empty(),
            None => false,
        },
        None => !name.is_empty(),
    }
}

/// The `<scheme>` of a `<scheme>:...` specifier, if it has one.
///
/// Deliberately lexical and deliberately narrow: a version range never
/// contains a colon, so anything that looks like a scheme is one. The shape is
/// the URI rule — a letter, then letters, digits, `+`, `.` or `-` — which is
/// what keeps `>=1.0.0` and `1.x` out of it while catching `git+ssh:`.
fn scheme_of(specifier: &str) -> Option<&str> {
    let (scheme, _) = specifier.split_once(':')?;
    let mut characters = scheme.chars();
    let first = characters.next()?;
    (first.is_ascii_alphabetic()
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')))
    .then_some(scheme)
}

/// Walk the dependency graph from every importer's declared ranges.
///
/// One walk covers the whole workspace rather than one per project, so two
/// importers asking for the same range select once and share the request. They
/// are deliberately not forced into agreement: importers wanting incompatible
/// versions each get their own node, exactly as two packages within one tree
/// do.
///
/// Kind-blind: a devDependency resolves exactly as a dependency does, and the
/// only thing that widened to admit them is this input. Nothing below decides
/// anything on a [`Kind`], which is what keeps "no registry package's dev
/// dependencies" a property of `VersionMetadata`'s shape rather than a rule
/// this walk has to remember.
pub fn resolve(
    registry: &dyn RegistryClient,
    roots: &BTreeMap<ImporterPath, BTreeMap<String, Declared>>,
    members: &BTreeMap<String, ImporterPath>,
) -> Result<ResolvedGraph, ResolveError> {
    let mut walk = Walk::seeded(registry, roots, members)?;
    walk.run()?;
    Ok(walk.into_graph())
}

/// One dependency walk: everything it accumulates, and the steps that move it.
///
/// These five collections are a set rather than five things that happen to sit
/// together — every step of the walk reads or writes several of them, and a
/// free function performing one step would take all five as `&mut` parameters.
/// They are private because nothing outside a walk has any business holding a
/// half-built graph: [`resolve`] is the only way to start one and
/// [`Walk::into_graph`] the only way to get anything out.
struct Walk<'a> {
    registry: &'a dyn RegistryClient,
    packages: BTreeMap<PackageId, ResolvedPackage>,
    importers: BTreeMap<ImporterPath, Importer>,
    /// One request per package, however many dependents ask for it.
    ///
    /// A `HashMap` rather than a `BTreeMap` deliberately: it is a memo that
    /// never reaches disk, so nothing about the result depends on the order it
    /// is filled in, which is what lets [`Walk::warm`] fill it from several
    /// threads at once.
    /// Each entry carries the freshness it was fetched under, so a name first
    /// seen as a range and later asked for by dist-tag is re-fetched rather
    /// than answered from a copy that was never required to be current.
    packuments: HashMap<String, (Packument, Freshness)>,
    /// Identical `(name, range)` pairs select once.
    selections: HashMap<(String, String), PackageId>,
    /// Edges discovered but not yet visited. Drained a level at a time.
    work: VecDeque<Pending>,
}

impl<'a> Walk<'a> {
    /// Start a walk from every importer's declared ranges.
    ///
    /// Local dependencies are settled here rather than in the walk: there is
    /// no tarball, no integrity hash and no version to select, so a
    /// `workspace:` specifier has nothing the walk could do with it. Only an
    /// importer may declare one — a registry package's dependencies are
    /// whatever it published, and `VersionMetadata` has no way to name a
    /// directory in this repo.
    fn seeded(
        registry: &'a dyn RegistryClient,
        roots: &BTreeMap<ImporterPath, BTreeMap<String, Declared>>,
        members: &BTreeMap<String, ImporterPath>,
    ) -> Result<Self, ResolveError> {
        let mut walk = Self {
            registry,
            packages: BTreeMap::new(),
            // Seeded from the input rather than filled in as edges arrive, so
            // an importer that declares nothing still appears in the graph —
            // it has a `node_modules` to own, and the lockfile records it as
            // resolved.
            importers: roots
                .keys()
                .map(|importer| (importer.clone(), Importer::default()))
                .collect(),
            packuments: HashMap::new(),
            selections: HashMap::new(),
            work: VecDeque::new(),
        };

        for (importer, declared) in roots {
            for (name, Declared { specifier, kind }) in declared {
                if !specifier.starts_with(WORKSPACE_PROTOCOL) {
                    walk.work.push_back(Pending {
                        dependent: Dependent::Importer {
                            path: importer.clone(),
                            kind: *kind,
                        },
                        name: name.clone(),
                        specifier: specifier.clone(),
                        asked: Asked::read(name, specifier)?,
                    });
                    continue;
                }

                let member = members
                    .get(name)
                    .ok_or_else(|| ResolveError::NoSuchMember {
                        name: name.clone(),
                        specifier: specifier.clone(),
                        members: members.keys().cloned().collect(),
                    })?;

                walk.importers
                    .entry(importer.clone())
                    .or_default()
                    .dependencies
                    .insert(
                        name.clone(),
                        Dependency {
                            specifier: specifier.clone(),
                            kind: *kind,
                            resolution: Resolution::Local(local_path(importer, member)),
                        },
                    );
            }
        }

        Ok(walk)
    }

    /// Walk until nothing is left to visit.
    ///
    /// A level at a time rather than an edge at a time: the whole frontier is
    /// taken, every packument it will ask for is fetched concurrently, and
    /// then the frontier is visited exactly as it was when each edge fetched
    /// its own. Draining the queue and refilling it preserves the FIFO order
    /// the single-edge loop had, so the walk visits the same edges in the same
    /// sequence — the concurrency is confined to the fetching.
    ///
    /// The cost of that confinement is a barrier per level: the slowest
    /// packument on one level holds up the next. A worklist drained by N
    /// workers would not have it, but it would also make `packuments` and
    /// `selections` shared mutable state, and with them the questions of which
    /// failure gets reported and whether discovery order can reach the
    /// lockfile. A tree's levels are few and wide — express is 69 packages in
    /// 7 levels — so the barrier costs a handful of round trips and buys back
    /// the entire design.
    fn run(&mut self) -> Result<(), ResolveError> {
        while !self.work.is_empty() {
            let level: Vec<Pending> = self.work.drain(..).collect();
            self.warm(&level);

            for pending in level {
                self.visit(pending)?;
            }
        }

        Ok(())
    }

    /// Resolve one edge: select a version, record the edge, and if the node is
    /// new, queue what it depends on.
    fn visit(&mut self, pending: Pending) -> Result<(), ResolveError> {
        let Pending {
            dependent,
            name,
            specifier,
            asked,
        } = pending;
        // Selection is keyed on the package asked for, not on the name it was
        // asked under, so two local names for one package share a node, a
        // packument and a store entry rather than each getting their own.
        let id = self.select(&asked.name, &asked.range)?;

        // Record the edge on whoever asked for it. An importer's own
        // dependency is recorded against the importer rather than against a
        // package, because that is what its `node_modules` is built from.
        match dependent {
            Dependent::Package(parent) => {
                if let Some(package) = self.packages.get_mut(&parent) {
                    package.dependencies.insert(name.clone(), id.clone());
                }
            }
            Dependent::Importer { path, kind } => {
                self.importers.entry(path).or_default().dependencies.insert(
                    name.clone(),
                    Dependency {
                        specifier: specifier.clone(),
                        kind,
                        resolution: Resolution::Registry(id.clone()),
                    },
                );
            }
        }

        // Recursion is gated on node novelty, not on path. That is what makes
        // a cycle terminate: the second visit finds the node present, records
        // the edge above, and stops here without needing a visited-path stack.
        if self.packages.contains_key(&id) {
            return Ok(());
        }

        let (packument, _) = self
            .packuments
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
        let resolved = metadata.dist.tarball.clone();

        // `dependencies` only. A dependency's `devDependencies` must never be
        // followed — doing so pulls in most of the registry — which is why
        // `VersionMetadata` has no field for them to be read from.
        for (dep_name, dep_specifier) in &metadata.dependencies {
            self.work.push_back(Pending {
                dependent: Dependent::Package(id.clone()),
                name: dep_name.clone(),
                specifier: dep_specifier.clone(),
                asked: Asked::read(dep_name, dep_specifier)?,
            });
        }

        self.packages.insert(
            id.clone(),
            ResolvedPackage {
                id,
                resolved,
                integrity,
                dependencies: BTreeMap::new(),
            },
        );

        Ok(())
    }

    /// Fetch every packument this level will ask for, concurrently, into the
    /// memo.
    ///
    /// Purely a warming pass: it decides nothing and records nothing but the
    /// memo, so [`Walk::visit`] behaves exactly as it did when it made these
    /// requests one at a time. That is what keeps the awkward questions from
    /// arriving with the concurrency. Which failure is reported stays the
    /// serial walk's answer, because a failure here is simply *not* a warm
    /// entry — the walk reaches that name in its own order, makes the request
    /// itself, and reports it there.
    ///
    /// Names are deduplicated, so the diamond that made the memo worth having
    /// is still fetched once even when both dependents sit on this level.
    /// Below two distinct names there is nothing to overlap and no pool is
    /// started.
    fn warm(&mut self, level: &[Pending]) {
        // The strictest requirement any edge on this level makes of each name.
        // One packument serves every edge that asked for the name, so if any
        // of them was a dist-tag the fetch has to satisfy that one.
        let mut wanted: BTreeMap<&str, Freshness> = BTreeMap::new();
        for pending in level {
            let name = pending.asked.name.as_str();
            let needed = Self::freshness_for(&pending.asked.range);
            if self.already_have(name, needed) {
                continue;
            }
            let entry = wanted.entry(name).or_insert(needed);
            if needed == Freshness::MustBeCurrent {
                *entry = Freshness::MustBeCurrent;
            }
        }
        let wanted: Vec<(&str, Freshness)> = wanted.into_iter().collect();

        if wanted.len() < 2 {
            return;
        }

        let registry = self.registry;
        let fetched: Mutex<Vec<(String, Packument, Freshness)>> = Mutex::new(Vec::new());

        // The error is discarded rather than propagated, and that is the whole
        // trick: see the note above on which failure a user sees. `drain`
        // stopping on the first failure is what keeps the wasted requests to
        // the names already claimed rather than the whole level.
        let _: Result<(), RegistryError> =
            pool::drain(&wanted, MAX_CONCURRENT_FETCHES, |(name, freshness)| {
                let packument = registry.packument(name, *freshness)?;
                fetched
                    .lock()
                    .unwrap()
                    .push(((*name).to_string(), packument, *freshness));
                Ok(())
            });

        // Insertion order cannot matter — the keys are distinct names and the
        // memo never reaches disk — which is why it is allowed to be a
        // `HashMap` at all.
        for (name, packument, freshness) in fetched.into_inner().unwrap() {
            self.packuments.insert(name, (packument, freshness));
        }
    }

    /// Is the memo already good enough for this requirement?
    ///
    /// An entry fetched under `MustBeCurrent` satisfies a later range, but not
    /// the other way round.
    fn already_have(&self, name: &str, needed: Freshness) -> bool {
        match self.packuments.get(name) {
            None => false,
            Some((_, Freshness::MustBeCurrent)) => true,
            Some((_, Freshness::MayBeCached)) => needed == Freshness::MayBeCached,
        }
    }

    /// What a spec requires of the registry.
    ///
    /// A range names a set, and a version that satisfied it a few hours ago
    /// satisfies it still, so a cached packument is a real answer. A dist-tag
    /// names whatever the registry means by it *today* — only the registry can
    /// say what `latest` points at — so it must be asked. A conditional
    /// request satisfies that: a `304` is the registry confirming, just now,
    /// that the stored copy is current.
    ///
    /// The test is the same one `select` uses to choose between a range and a
    /// tag, and deliberately so: anything `Range::parse` accepts is resolved
    /// as a range there, so anything it accepts may be cached here.
    fn freshness_for(range: &str) -> Freshness {
        if Range::parse(range).is_ok() {
            Freshness::MayBeCached
        } else {
            Freshness::MustBeCurrent
        }
    }

    /// Choose the concrete version satisfying one `(name, range)` pair,
    /// fetching and caching the packument as needed.
    fn select(&mut self, name: &str, range: &str) -> Result<PackageId, ResolveError> {
        let key = (name.to_string(), range.to_string());
        if let Some(id) = self.selections.get(&key) {
            return Ok(id.clone());
        }

        let needed = Self::freshness_for(range);
        if !self.already_have(name, needed) {
            let packument = self.registry.packument(name, needed)?;
            self.packuments
                .insert(name.to_string(), (packument, needed));
        }
        let packument = &self.packuments[name].0;

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
        self.selections.insert(key, id.clone());
        Ok(id)
    }

    fn into_graph(self) -> ResolvedGraph {
        ResolvedGraph {
            importers: self.importers,
            packages: self.packages,
        }
    }
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
