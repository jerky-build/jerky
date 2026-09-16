//! Transitive dependency resolution.
//!
//! Pure given a [`RegistryClient`]: this module fetches metadata and returns a
//! graph, writing nothing to disk. That is what lets the tree-shape tests run
//! with no filesystem at all, and it is why the lockfile can be a straight
//! serialization of the result.
//!
//! A resolution is two passes over the same graph, and the split is the answer
//! to a question rather than an implementation detail. **The crawl** — see
//! [`Walk::prefetch`] — is a continuous worklist: it descends the graph on a
//! pool of threads, and the moment a packument lands it schedules the names
//! that packument reveals, so the whole thing is paced by the graph's longest
//! chain rather than by its depth times the slowest fetch on each level.
//! **The walk** — [`Walk::run`] — then builds the graph on one thread, in one
//! fixed order, out of a memo that is already full.
//!
//! Keeping them apart is what makes completion order unable to reach the
//! lockfile. Every decision — which version satisfies a range, which edge is
//! recorded where, which failure is reported — is taken by the walk, which
//! visits in the order it always has and cannot tell how its memo got full.
//!
//! That argument has a trap in it, and it is worth stating because the obvious
//! version of it is wrong. It is *not* enough for the memo to converge on the
//! same contents whatever order a fixed set of requests completes in, because
//! **the set of requests is itself order-dependent**: a crawl worker that
//! reads a cached packument can select a version whose dependencies the
//! resolved graph never contains, and go on to ask the registry about names,
//! or at freshnesses, that nothing real ever wanted. What is needed is that
//! such an ask cannot *disturb* anything — and that is why [`Memo`] is keyed
//! by [`Request`], a package **and** the freshness asked of it, rather than by
//! package alone. Each entry is then the answer to its own key and to nothing
//! else, so a spurious request adds an entry nobody reads instead of changing
//! one somebody does.
//!
//! **Not done here, and cheap now:** tarball fetching still waits for the
//! whole of resolution to finish. The crawl already holds each selected
//! version and its `dist.tarball` at the moment that version's packument
//! lands, so the information needed to start a download arrives long before
//! the walk does — overlapping the two phases no longer needs anything
//! discovered that is not already in hand. The barrier itself lives in
//! `commands::install`, which is why it is only noted here.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use thiserror::Error;

use crate::integrity::{Integrity, IntegrityError};
use crate::pool;
use crate::range::{Range, RangeError, Version};
use crate::registry::{
    Freshness, MAX_CONCURRENT_FETCHES, Packument, RegistryClient, RegistryError, VersionMetadata,
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

/// The longest a rendered [`PackageId`] may be before its peer context is
/// replaced by a hash.
///
/// The rendered form is a directory name, and a single path component cannot
/// exceed 255 bytes on the filesystems jerky targets. 200 leaves headroom
/// rather than sitting on the limit. Hashing costs readability — a lockfile of
/// hashed keys is one nobody can review — so it is a fallback and never the
/// default.
const MAX_KEY_BYTES: usize = 200;

/// How much of the digest a collapsed peer suffix keeps. Eight bytes is
/// sixteen hex characters — short enough to stay readable in a key, wide
/// enough that two contexts colliding is not a thing that happens.
const PEER_HASH_BYTES: usize = 8;

/// A node in the resolved graph: one concrete version of one package, resolved
/// against a particular set of peers.
///
/// The peer context is part of the identity because it has to be. With no
/// ambient hoisting, `react-dom@18.2.0` given `react@18.2.0` and the same
/// `react-dom@18.2.0` given `react@17.0.2` need different directories — the
/// link at `node_modules/react` inside each differs — so one `name@version`
/// cannot address both.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageId {
    pub name: String,
    pub version: String,
    /// What this node resolved *against* — the whole of what distinguishes it
    /// from another copy of the same published version.
    ///
    /// Two kinds of entry, for one reason. The first is this node's own
    /// resolved peers, keyed by the name it declared them under. The second is
    /// each dependency that itself carries a context, keyed by the name this
    /// node calls it.
    ///
    /// The second kind is what makes a peer-less node duplicable at all. A
    /// package that declares no peers but depends on one that does must still
    /// become two nodes when its dependency does — otherwise both copies key
    /// the same and `ResolvedGraph::packages` silently keeps one, dropping a
    /// whole subtree. Folding the dependency's context in is what gives the
    /// two copies different keys, and it is why this is a *context* and not a
    /// peer map.
    ///
    /// Values are full `PackageId`s rather than version strings, so two
    /// contexts differing only deep inside are still different contexts.
    ///
    /// Empty for the overwhelming majority of packages, and empty is what
    /// makes this field invisible to them.
    pub context: BTreeMap<String, PackageId>,
}

impl PackageId {
    /// A package resolved against no peers — every node the walk produces,
    /// before the peer pass has anything to say about it.
    pub fn plain(name: impl Into<String>, version: impl Into<String>) -> Self {
        PackageId {
            name: name.into(),
            version: version.into(),
            context: BTreeMap::new(),
        }
    }

    /// The context suffix alone, rendered in full: `(react@18.2.0)`, nested.
    ///
    /// Parenthesised rather than delimited because the context nests, and a
    /// flat separator cannot say whether the third name is a peer of the first
    /// or a peer of the second.
    ///
    /// The recursion terminates unconditionally: `peers` owns its values, so
    /// the structure is necessarily a finite tree. A *graph* of peers can hold
    /// a cycle, and breaking it is the job of whatever builds these ids — it
    /// cannot be represented here to begin with.
    fn render_context(&self, out: &mut String) {
        // Sorted by the name the dependent declared, which is `BTreeMap`'s
        // iteration order, so two machines render identically.
        for peer in self.context.values() {
            out.push('(');
            // A scoped peer's `/` would open a directory level. The base name
            // is allowed one — the linker already expects `@types/node@20.0.0`
            // to be two components — but the suffix must stay inside the last.
            out.push_str(&peer.name.replace('/', "+"));
            out.push('@');
            out.push_str(&peer.version);
            peer.render_context(out);
            out.push(')');
        }
    }

    /// A stable short hash of an already-rendered context suffix.
    ///
    /// Takes the rendering rather than re-deriving it, so there is one walk
    /// and no way for the hashed context and the readable one to drift.
    fn context_hash(suffix: &str) -> String {
        use sha2::Digest as _;

        let digest = sha2::Sha512::digest(suffix.as_bytes());
        use std::fmt::Write as _;
        digest[..PEER_HASH_BYTES]
            .iter()
            .fold(String::new(), |mut out, byte| {
                let _ = write!(out, "{byte:02x}");
                out
            })
    }
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The no-peer spelling is byte-identical to what this rendered before
        // peers existed. That is a requirement, not a coincidence: it is what
        // keeps the format change invisible to every package without peers.
        if self.context.is_empty() {
            return write!(f, "{}@{}", self.name, self.version);
        }

        let mut suffix = String::new();
        self.render_context(&mut suffix);

        let rendered = format!("{}@{}{}", self.name, self.version, suffix);
        if rendered.len() <= MAX_KEY_BYTES {
            return f.write_str(&rendered);
        }

        // `_` marks a collapsed suffix. It cannot be confused with the
        // readable form, which always opens with `(`, and a version carries no
        // `_` of its own.
        write!(
            f,
            "{}@{}_{}",
            self.name,
            self.version,
            Self::context_hash(&suffix)
        )
    }
}

/// Read a version's published peers into the graph's own shape.
///
/// `peerDependenciesMeta` may name a peer that `peerDependencies` does not
/// declare. That is meaningless rather than malformed — the registry has
/// published worse — so the declaration drives the pairing and a meta entry
/// with nothing to flag is simply never read.
fn declared_peers(metadata: &VersionMetadata) -> BTreeMap<String, DeclaredPeer> {
    metadata
        .peer_dependencies
        .iter()
        .map(|(name, range)| {
            let optional = metadata
                .peer_dependencies_meta
                .get(name)
                .is_some_and(|meta| meta.optional);
            (
                name.clone(),
                DeclaredPeer {
                    range: range.clone(),
                    optional,
                },
            )
        })
        .collect()
}

/// Split a rendered [`PackageId`] key into its name and version, discarding
/// any peer suffix.
///
/// The suffix is dropped rather than returned because nothing reads it: it
/// exists only to keep two peer resolutions of one version apart, and what
/// those peers were is recorded in a field of its own. That is also what lets
/// the collapsed form be an opaque hash at all.
///
/// Delegates to [`split_name_and_version`] rather than splitting again, so the
/// crate keeps one decoder to match its one encoder. That function is shared
/// with `alias_target` and deliberately knows nothing about peers; all this
/// adds is finding where the suffix begins.
pub(crate) fn split_key(key: &str) -> Option<(&str, &str)> {
    // A readable suffix always opens `(`, and a package name may not contain
    // one. A collapsed suffix is `_` and a fixed run of hex at the very end —
    // anchored there because a name *may* contain `_`, so nothing earlier in
    // the key can be assumed to be the marker.
    let head = match key.find('(') {
        Some(open) => &key[..open],
        None => strip_collapsed_suffix(key),
    };

    split_name_and_version(head)
}

/// `p@1.0.0_0123456789abcdef` -> `p@1.0.0`, and anything else unchanged.
fn strip_collapsed_suffix(key: &str) -> &str {
    let Some((head, tail)) = key.rsplit_once('_') else {
        return key;
    };

    let collapsed =
        tail.len() == PEER_HASH_BYTES * 2 && tail.bytes().all(|byte| byte.is_ascii_hexdigit());

    if collapsed { head } else { key }
}

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub id: PackageId,
    /// The tarball URL, carried through so the lockfile records it.
    pub resolved: String,
    pub integrity: Integrity,
    /// What this package calls a dependency -> the node it resolved to.
    pub dependencies: BTreeMap<String, PackageId>,
    /// What this package requires of its consumer, exactly as published.
    ///
    /// Carried rather than resolved by the walk, because a peer is not an edge
    /// to follow: it is a constraint answered by whoever installs this package,
    /// and the walk has no idea who that is. The peer pass does.
    ///
    /// Recorded even for packages whose peers all turn out to be satisfied, so
    /// that a diagnostic can be recomputed later from the graph alone.
    pub declared_peers: BTreeMap<String, DeclaredPeer>,
}

/// One `peerDependencies` entry, with the flag `peerDependenciesMeta` carries
/// for it.
///
/// A pair rather than two parallel maps, because a range with no optionality
/// and an optionality with no range are both meaningless, and keeping them
/// together means no consumer can read one and forget the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredPeer {
    pub range: String,
    /// `peerDependenciesMeta[name].optional`. An unsatisfied optional peer is
    /// silent; an unsatisfied required one is a diagnostic.
    pub optional: bool,
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
    walk.prefetch();
    walk.run()?;
    Ok(walk.into_graph())
}

/// A required peer that nothing in the dependent's environment satisfies.
///
/// Produced here and printed elsewhere: the resolver has no business writing to
/// a terminal, and the install command already returns its warnings for
/// `main` to render.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnsatisfiedPeer {
    /// The package that declared the peer, as it was reached.
    pub dependent: PackageId,
    /// The name it declared, and the range it asked of it.
    pub peer: String,
    pub range: String,
    /// The version of the provider that was found, when one was found at all.
    ///
    /// `None` is "nothing provides this"; `Some` is "something does, and it is
    /// the wrong version". The two are different problems and read as different
    /// sentences, which is the whole reason this is an `Option` rather than a
    /// bool saying it went wrong.
    pub found: Option<String>,
}

/// Resolve every package's peers, duplicating nodes whose answers differ.
///
/// A pure graph-to-graph pass. It fetches nothing and touches no filesystem,
/// which is what lets it be tested exactly the way the walk is — and it is
/// sound *only* because jerky never fabricates an edge: a peer is satisfied
/// from what is already resolved, so peer resolution can never cause a new
/// version to be selected, and therefore can run after selection is finished.
///
/// The returned graph is reachable by construction: it is rebuilt from the
/// importers down, so a node nothing reaches is simply never emitted.
pub fn resolve_peers(graph: ResolvedGraph) -> (ResolvedGraph, Vec<UnsatisfiedPeer>) {
    let mut pass = PeerPass {
        needed: peer_names_needed(&graph),
        source: &graph,
        packages: BTreeMap::new(),
        resolved: BTreeMap::new(),
        in_progress: BTreeSet::new(),
        unsatisfied: BTreeSet::new(),
    };

    let mut importers = graph.importers.clone();
    for importer in importers.values_mut() {
        // The importer's own dependencies are the outermost frame: the last
        // place a peer looks, and the only one a top-level package has.
        let provided: BTreeMap<String, PackageId> = importer
            .dependencies
            .iter()
            .filter_map(|(name, dependency)| match &dependency.resolution {
                Resolution::Registry(id) => Some((name.clone(), id.clone())),
                Resolution::Local(_) => None,
            })
            .collect();

        for dependency in importer.dependencies.values_mut() {
            if let Resolution::Registry(id) = &dependency.resolution {
                let resolved = pass.visit(id, &[&provided]);
                dependency.resolution = Resolution::Registry(resolved);
            }
        }
    }

    let unsatisfied = pass.unsatisfied.iter().cloned().collect();
    let packages = pass.packages;

    (
        ResolvedGraph {
            importers,
            packages,
        },
        unsatisfied,
    )
}

/// The peer pass's working state.
///
/// `source` is the peer-blind graph being read; `packages` is the re-keyed one
/// being built. They are separate because a node's new key is not known until
/// its whole subtree is, so rewriting in place would mean holding a graph whose
/// keys and contents disagree.
struct PeerPass<'a> {
    source: &'a ResolvedGraph,
    /// Every peer name each node's subtree can ask about, itself included.
    ///
    /// This is what a node is keyed on, and the reason it has to exist: a
    /// package declaring no peers of its own is still duplicated by one deeper
    /// down, so "what did *this* node resolve" is not enough to tell two copies
    /// apart. What separates them is what the subtree beneath them resolved,
    /// and this names the only part of the environment that can affect it.
    needed: BTreeMap<PackageId, BTreeSet<String>>,
    packages: BTreeMap<PackageId, ResolvedPackage>,
    /// `(node as published, the environment its subtree can see)` -> the node
    /// it became.
    ///
    /// Ordered rather than hashed only because `PackageId` is `Ord` and not
    /// `Hash`; nothing here depends on the order.
    resolved: BTreeMap<PeerKey, PackageId>,
    /// The same keys, while their subtrees are still being walked. This is what
    /// makes a peer cycle terminate, and it is the same novelty gate the walk
    /// uses, so the design carries one termination argument rather than two.
    in_progress: BTreeSet<PeerKey>,
    /// A set rather than a list: one node reached through several paths
    /// reports the same complaint each time, and a package manager that says
    /// the same sentence eleven times has told the user nothing extra.
    unsatisfied: BTreeSet<UnsatisfiedPeer>,
}

impl PeerPass<'_> {
    /// Resolve one node in the context of the path that reached it, returning
    /// the id it became.
    ///
    /// `providers` is the chain of dependency maps from the importer down to
    /// this node's parent, outermost first.
    fn visit(&mut self, id: &PackageId, providers: &[&BTreeMap<String, PackageId>]) -> PackageId {
        let Some(package) = self.source.packages.get(id) else {
            // Nothing in a graph the walk produced, but a hand-written lockfile
            // can name an edge it does not record. Leaving the id alone lets
            // the lockfile's own validation report that, rather than this pass
            // panicking on it first.
            return id.clone();
        };

        // Keyed on the environment the whole subtree can see, not merely on
        // what this node resolved for itself. Two visits agreeing on every
        // peer name the subtree could ask about must produce the same subtree,
        // and two that differ anywhere in it must not share a node.
        let relevant: BTreeMap<String, PackageId> = self
            .needed
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|name| {
                lookup(name, &package.dependencies, providers).map(|found| (name.clone(), found))
            })
            .collect();
        let key = (id.clone(), relevant);

        if let Some(done) = self.resolved.get(&key) {
            return done.clone();
        }

        let own = self.own_peers(package, providers);

        if self.in_progress.contains(&key) {
            // A peer cycle. The node is already being built further up this
            // path; give the caller the identity its own peers imply and stop,
            // rather than descending into it a second time. Folding this
            // subtree's context in would require the answer this call is
            // still computing.
            return PackageId {
                name: id.name.clone(),
                version: id.version.clone(),
                context: own,
            };
        }
        self.in_progress.insert(key.clone());

        // This node's own dependencies are what the level below looks at first
        // among its ancestors.
        let mut chain: Vec<&BTreeMap<String, PackageId>> = providers.to_vec();
        chain.push(&package.dependencies);

        let mut dependencies = BTreeMap::new();
        for (name, dep_id) in &package.dependencies {
            dependencies.insert(name.clone(), self.visit(dep_id, &chain));
        }

        // The context is the node's own peers, plus every dependency that
        // itself carries one. The second half is what duplicates a package
        // that declares no peers at all: without it, two copies pointing at
        // different subtrees would key identically and the graph would keep
        // one.
        let mut context = own;
        for (name, dep_id) in &dependencies {
            if !dep_id.context.is_empty() {
                context.insert(name.clone(), dep_id.clone());
            }
        }

        let resolved = PackageId {
            name: id.name.clone(),
            version: id.version.clone(),
            context,
        };

        self.packages.insert(
            resolved.clone(),
            ResolvedPackage {
                id: resolved.clone(),
                resolved: package.resolved.clone(),
                integrity: package.integrity.clone(),
                dependencies,
                declared_peers: package.declared_peers.clone(),
            },
        );

        self.in_progress.remove(&key);
        self.resolved.insert(key, resolved.clone());
        resolved
    }

    /// Which provider answers each of this package's declared peers.
    ///
    /// Own dependencies first — a package declaring the same name in both
    /// `dependencies` and `peerDependencies` is saying "I will take yours, but
    /// I ship a fallback", and its own copy is the one it gets. Then the
    /// nearest ancestor, and the importer last, which falls out of walking the
    /// chain innermost-first.
    fn own_peers(
        &mut self,
        package: &ResolvedPackage,
        providers: &[&BTreeMap<String, PackageId>],
    ) -> BTreeMap<String, PackageId> {
        let mut own = BTreeMap::new();

        for (name, declared) in &package.declared_peers {
            let provider = lookup(name, &package.dependencies, providers);

            let Some(provider) = provider else {
                if !declared.optional {
                    self.unsatisfied.insert(UnsatisfiedPeer {
                        dependent: package.id.clone(),
                        peer: name.clone(),
                        range: declared.range.clone(),
                        found: None,
                    });
                }
                continue;
            };

            if satisfies(&provider.version, &declared.range) {
                own.insert(name.clone(), provider);
                continue;
            }

            // Out of range is a diagnostic even for an optional peer: `optional`
            // says the peer may be absent, not that any version of it will do.
            self.unsatisfied.insert(UnsatisfiedPeer {
                dependent: package.id.clone(),
                peer: name.clone(),
                range: declared.range.clone(),
                found: Some(provider.version),
            });
        }

        own
    }
}

/// What a node is keyed on while the pass runs: the package as published, and
/// the providers its subtree can reach for every peer name that subtree names.
type PeerKey = (PackageId, BTreeMap<String, PackageId>);

/// Find who provides `name`, in the order the rule requires: the package's own
/// dependencies first, then the nearest ancestor, then the importer.
///
/// `providers` runs outermost-first, so walking it in reverse is walking back
/// up the tree from nearest to furthest.
fn lookup(
    name: &str,
    own: &BTreeMap<String, PackageId>,
    providers: &[&BTreeMap<String, PackageId>],
) -> Option<PackageId> {
    own.get(name)
        .or_else(|| providers.iter().rev().find_map(|frame| frame.get(name)))
        .cloned()
}

/// Every peer name each node's subtree can ask about, itself included.
///
/// A least fixed point rather than a recursive walk, so a dependency cycle
/// costs an extra round instead of a stack overflow. The sets only grow and
/// the names are finite, so it terminates.
fn peer_names_needed(graph: &ResolvedGraph) -> BTreeMap<PackageId, BTreeSet<String>> {
    let mut needed: BTreeMap<PackageId, BTreeSet<String>> = graph
        .packages
        .iter()
        .map(|(id, package)| (id.clone(), package.declared_peers.keys().cloned().collect()))
        .collect();

    loop {
        let mut changed = false;

        for (id, package) in &graph.packages {
            let mut grown = needed[id].clone();
            for dependency in package.dependencies.values() {
                if let Some(below) = needed.get(dependency) {
                    grown.extend(below.iter().cloned());
                }
            }

            if grown != needed[id] {
                needed.insert(id.clone(), grown);
                changed = true;
            }
        }

        if !changed {
            return needed;
        }
    }
}

/// Does this version satisfy this range?
///
/// An unparseable range fails closed. A peer range is whatever the publisher
/// typed, and treating nonsense as satisfied would silently record a peer
/// resolution nobody asked for; reporting it is the honest answer.
fn satisfies(version: &str, range: &str) -> bool {
    let (Ok(version), Ok(range)) = (Version::parse(version), Range::parse(range)) else {
        return false;
    };

    range
        .max_satisfying(std::slice::from_ref(&version))
        .is_some()
}

/// One question for the registry: which package, and how current the answer
/// has to be.
///
/// A *pair*, and the pair is the whole of what makes the memo below safe. It
/// is tempting to key a packument cache on the name alone, because a name is
/// what a registry addresses — but with the metadata cache behind it the two
/// freshnesses are two different answers for one name, since a release
/// published inside the cache's window is in one and not the other. Keyed on
/// the name, "what is in the memo for `y`" would depend on whether anything
/// had happened to ask for `y` by dist-tag; keyed on the pair, it does not.
///
/// The freshness is carried as a `bool` only because [`Freshness`] is
/// `registry`'s type and does not derive `Hash`. `must_be_current` is the
/// whole of what it says.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Request {
    name: String,
    must_be_current: bool,
}

impl Request {
    fn new(name: &str, needed: Freshness) -> Self {
        Self {
            name: name.to_string(),
            must_be_current: matches!(needed, Freshness::MustBeCurrent),
        }
    }
}

/// One packument per distinct request, shared by the crawl and the walk.
///
/// The one piece of state several threads touch, and it is deliberately the
/// only one. Each entry is the registry's answer to its own key and to
/// nothing else, so no entry can be changed — or brought into existence — by
/// what some other caller happened to ask for. That is a stronger property
/// than "the memo ends up the same", and it is the one that is actually
/// needed: the crawl's set of asks is *itself* order-dependent, because a
/// worker that reads a cached packument can select a version that names
/// dependencies the resolved graph never contains. Those asks must be unable
/// to disturb anything, not merely unable to disagree.
///
/// This is what confines the worklist's nondeterminism somewhere it cannot
/// escape from, and it is why a `HashMap` is allowed here at all despite
/// everything downstream of it reaching the lockfile.
///
/// The cost is that a name asked both ways is fetched twice. That is the
/// honest price of the two questions being different: the second fetch is the
/// one the dist-tag asked for and could not have been given from the first.
#[derive(Default)]
struct Memo {
    state: Mutex<MemoState>,
    landed: Condvar,
}

#[derive(Default)]
struct MemoState {
    packuments: HashMap<Request, Arc<Packument>>,
    /// Requests some thread is making *right now*.
    ///
    /// Without this the crawl loses the property that one request is one
    /// fetch. Two dependents asking for two different ranges of the same name
    /// are two items on the worklist and one request, so two workers reach the
    /// memo together, both miss, and both fetch — a diamond paid for twice, on
    /// a real tree several hundred times over.
    claimed: HashSet<Request>,
}

impl Memo {
    /// The packument answering one request, fetching it if the memo has not
    /// got it already.
    ///
    /// A caller that finds the request already claimed waits for the claim to
    /// settle rather than starting a second fetch of the same thing. Waiting
    /// cannot deadlock: whoever holds a claim is not waiting on anything here,
    /// so some thread is always making progress on it.
    ///
    /// Note what is deliberately *not* here: an entry fetched under
    /// `MustBeCurrent` is not offered to a later range. It would save a
    /// request, and it is what this module used to do, and it is precisely the
    /// coupling that let one edge's freshness decide another edge's answer.
    /// A range asks whether a cached copy will do; being handed a current one
    /// instead is a different question answered.
    fn obtain(
        &self,
        registry: &dyn RegistryClient,
        name: &str,
        needed: Freshness,
    ) -> Result<Arc<Packument>, RegistryError> {
        let request = Request::new(name, needed);

        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(packument) = state.packuments.get(&request) {
                return Ok(Arc::clone(packument));
            }
            if !state.claimed.contains(&request) {
                break;
            }
            state = self.landed.wait(state).unwrap();
        }
        state.claimed.insert(request.clone());
        drop(state);

        let fetched = registry.packument(name, needed);

        let mut state = self.state.lock().unwrap();
        state.claimed.remove(&request);
        let answer = fetched.map(|packument| {
            let packument = Arc::new(packument);
            state.packuments.insert(request, Arc::clone(&packument));
            packument
        });
        drop(state);
        // A failure is released as well as a success. Nothing is recorded for
        // it, so a waiter wakes, misses, and makes the request itself — which
        // is what puts the failure in front of whoever can report it usefully.
        self.landed.notify_all();

        answer
    }
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
    /// One request per package, however many dependents ask for it. The only
    /// field the crawl touches, and the only one behind a lock.
    memo: Memo,
    /// Identical `(name, range)` pairs select once. Owned by the walk thread,
    /// which is the whole of what "selection stays single-threaded" means
    /// here: nothing shared decides a version.
    selections: HashMap<(String, String), PackageId>,
    /// Edges discovered but not yet visited, oldest first.
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
            memo: Memo::default(),
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

    /// Fetch every packument the walk will ask for, as a continuous worklist.
    ///
    /// One item is one `(name, range)` pair. Fetching its packument is what
    /// reveals the pairs below it, so those go on the list the moment the
    /// fetch lands and are claimed by whichever worker is free — there is no
    /// point at which the crawl waits for a level to finish. A chain of ten
    /// costs ten round trips because it is ten round trips; what it no longer
    /// costs is ten round trips *for everything else in the tree too*.
    ///
    /// **It decides nothing, and that is the load-bearing claim.** It writes
    /// only [`Walk::memo`], whose contents are keyed by name and therefore
    /// identical however the crawl was scheduled. It reads a version out of
    /// each packument, but only to know which dependencies to ask for next —
    /// the answer is thrown away, and [`Walk::select`] computes it again on
    /// the walk thread from the same packument with the same function. So the
    /// graph is not a function of this at all: run the crawl twice, or not at
    /// all, and the walk produces the same bytes.
    ///
    /// Failures are dropped for the same reason the level-warming pass before
    /// it dropped them. A name the crawl could not fetch is simply not in the
    /// memo; the walk reaches it in its own fixed order, asks for it itself,
    /// and reports the failure from there — which is what keeps "which error
    /// does a user see" a question about the walk's order rather than about
    /// which worker lost a race.
    ///
    /// Dropped, but not ignored: a registry failure stops the crawl. The level
    /// pass got that for free from `pool::drain`, and losing it would mean a
    /// typo'd dependency name fetching the entire rest of the graph before
    /// anything was reported — several thousand requests to say that one of
    /// them was a 404. Stopping cannot affect the answer, only the work: a
    /// half-full memo is one the walk fills in itself, in its own order.
    /// A crawl stopped by a failure the walk never reaches costs some
    /// prefetching and nothing else.
    fn prefetch(&self) {
        // Seeded from the importers' own declarations, which is the same set
        // of edges `run` starts from.
        let mut scouted: HashSet<(String, String)> = HashSet::new();
        let seeds: Vec<Asked> = self
            .work
            .iter()
            .map(|pending| pending.asked.clone())
            .filter(|asked| scouted.insert((asked.name.clone(), asked.range.clone())))
            .collect();
        let scouted = Mutex::new(scouted);

        pool::crawl(seeds, MAX_CONCURRENT_FETCHES, |asked, work| {
            let needed = Self::freshness_for(&asked.range);
            let Ok(packument) = self.memo.obtain(self.registry, &asked.name, needed) else {
                // The walk is going to stop at this name, or at one before it.
                // Either way the rest of the graph is prefetching for a
                // resolution that is not going to happen.
                work.stop();
                return;
            };
            let Ok(version) = choose(&packument, &asked.name, &asked.range) else {
                return;
            };
            // `choose` refuses a version the packument does not publish, on
            // both of its paths, so this cannot miss.
            let Some(metadata) = packument.versions.get(&version) else {
                return;
            };

            // `dependencies` only, exactly as `visit` reads them: a
            // dependency's `devDependencies` must never be followed, and
            // `VersionMetadata` has no field they could arrive through.
            for (name, specifier) in &metadata.dependencies {
                let Ok(next) = Asked::read(name, specifier) else {
                    continue;
                };
                let unseen = scouted
                    .lock()
                    .unwrap()
                    .insert((next.name.clone(), next.range.clone()));
                if unseen {
                    work.push(next);
                }
            }
        });
    }

    /// Walk until nothing is left to visit.
    ///
    /// One edge at a time, oldest first, on this thread alone. After
    /// [`Walk::prefetch`] every packument it wants is already in the memo, so
    /// what this loop costs is arithmetic rather than round trips — but it is
    /// written as though the memo were empty, and on the failure path it is:
    /// a name the crawl could not fetch is requested here, and reported here.
    ///
    /// The order is the order the walk has always had, which is what makes the
    /// error a user sees the same one on every run. It is also why there is no
    /// machinery in this module for reconciling failures discovered out of
    /// order — the only pass that can discover one is this one, and it takes
    /// them one at a time.
    fn run(&mut self) -> Result<(), ResolveError> {
        while let Some(pending) = self.work.pop_front() {
            self.visit(pending)?;
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
        let (id, packument) = self.select(&asked.name, &asked.range)?;

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

        // Both paths in `choose` check membership before returning: the range
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
                declared_peers: declared_peers(metadata),
            },
        );

        Ok(())
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

    /// Choose the concrete version satisfying one `(name, range)` pair, and
    /// hand back the packument it was chosen from.
    ///
    /// The packument comes back with it because [`Walk::visit`] needs the same
    /// one to read the chosen version's tarball, integrity and edges. Looking
    /// it up a second time would be a second chance to disagree about which
    /// freshness was asked for, on a path where disagreeing means recording a
    /// hash from one copy and a URL from another.
    ///
    /// Only ever called on the walk thread. The memo may have been filled by
    /// the crawl, but which version a range selects is decided here and
    /// nowhere else.
    fn select(
        &mut self,
        name: &str,
        range: &str,
    ) -> Result<(PackageId, Arc<Packument>), ResolveError> {
        let packument = self
            .memo
            .obtain(self.registry, name, Self::freshness_for(range))?;

        let key = (name.to_string(), range.to_string());
        if let Some(id) = self.selections.get(&key) {
            return Ok((id.clone(), packument));
        }

        let version = choose(&packument, name, range)?;
        // Peer-free by construction. The walk resolves versions and nothing
        // else; peers are settled afterwards, over the finished graph, which
        // is sound precisely because a peer is satisfied only from what is
        // already resolved and so can never change a selection.
        let id = PackageId::plain(name, version);
        self.selections.insert(key, id.clone());
        Ok((id, packument))
    }

    fn into_graph(self) -> ResolvedGraph {
        ResolvedGraph {
            importers: self.importers,
            packages: self.packages,
        }
    }
}

/// Which version of `packument` a spec selects.
///
/// A free function and a pure one, because two passes have to agree about it
/// exactly: [`Walk::select`] takes the answer as the graph's, and
/// [`Walk::prefetch`] takes it only to know which dependencies to ask the
/// registry for next. Two spellings of this rule would be two chances for the
/// crawl to fetch a version's dependencies that the walk then does not use —
/// which would not be wrong, only wasteful, and silently so.
///
/// Range syntax is tried first, and a dist-tag is only the fallback for a spec
/// that is not a range at all. npm resolves in this order for a reason: were
/// tags consulted first, a registry could publish a tag named `^1.0.0` and
/// silently override what that range means.
fn choose(packument: &Packument, name: &str, range: &str) -> Result<String, ResolveError> {
    match Range::parse(range) {
        Ok(parsed) => {
            let available = packument.versions_sorted();
            match parsed.max_satisfying(&available) {
                Some(chosen) => Ok(chosen.as_str().to_string()),
                None => Err(ResolveError::Unsatisfiable {
                    name: name.to_string(),
                    range: range.to_string(),
                    available: available.iter().map(|v| v.as_str().to_string()).collect(),
                }),
            }
        }
        Err(_) => match packument.resolve_tag(range) {
            Some(tagged) => {
                // A tag is a pointer the registry maintains, and it can dangle:
                // unpublishing a version leaves the tag behind. Trusting it
                // blindly would panic on the lookup the caller makes next.
                if !packument.versions.contains_key(tagged) {
                    return Err(ResolveError::DanglingTag {
                        name: name.to_string(),
                        tag: range.to_string(),
                        version: tagged.to_string(),
                    });
                }
                Ok(tagged.to_string())
            }
            None => Err(ResolveError::UnresolvableSpec {
                name: name.to_string(),
                spec: range.to_string(),
                tags: packument.dist_tags.keys().cloned().collect(),
            }),
        },
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

    /// A registry package with no peers, which is nearly all of them.
    fn plain(name: &str, version: &str) -> PackageId {
        PackageId::plain(name, version)
    }

    fn with_peers(name: &str, version: &str, peers: &[PackageId]) -> PackageId {
        PackageId {
            name: name.to_string(),
            version: version.to_string(),
            context: peers
                .iter()
                .map(|peer| (peer.name.clone(), peer.clone()))
                .collect(),
        }
    }

    #[test]
    fn an_empty_peer_context_renders_as_name_at_version() {
        // The requirement the rest of the format rests on. Nearly every
        // package has no peers, so this is what keeps the change invisible to
        // them and keeps every key already asserted elsewhere asserting the
        // same string.
        assert_eq!(plain("lodash", "4.17.21").to_string(), "lodash@4.17.21");
        assert_eq!(
            plain("@types/node", "20.0.0").to_string(),
            "@types/node@20.0.0"
        );
    }

    #[test]
    fn a_single_peer_renders_in_parentheses() {
        let id = with_peers("react-dom", "18.2.0", &[plain("react", "18.2.0")]);
        assert_eq!(id.to_string(), "react-dom@18.2.0(react@18.2.0)");
    }

    #[test]
    fn peers_render_sorted_whatever_the_insertion_order() {
        // Two machines resolving the same tree must serialize identically,
        // which is the same reason the graph uses BTreeMap throughout.
        let forwards = with_peers("p", "1.0.0", &[plain("a", "1.0.0"), plain("b", "2.0.0")]);
        let backwards = with_peers("p", "1.0.0", &[plain("b", "2.0.0"), plain("a", "1.0.0")]);
        assert_eq!(forwards.to_string(), "p@1.0.0(a@1.0.0)(b@2.0.0)");
        assert_eq!(forwards.to_string(), backwards.to_string());
    }

    #[test]
    fn a_scoped_peer_stays_inside_one_path_component() {
        // The rendered id is a directory name. The base name's `/` already
        // makes two components and the linker knows it; a peer's must not add
        // a third, so it is written `+`.
        let id = with_peers("p", "1.0.0", &[plain("@babel/core", "7.24.0")]);
        assert_eq!(id.to_string(), "p@1.0.0(@babel+core@7.24.0)");
    }

    #[test]
    fn a_peer_carrying_its_own_peers_renders_nested() {
        // Nesting is why the suffix is parenthesised rather than delimited: a
        // flat separator cannot say whether the third name is a peer of the
        // first or of the second.
        let inner = with_peers("b", "2.0.0", &[plain("c", "3.0.0")]);
        let id = with_peers("a", "1.0.0", &[inner, plain("d", "4.0.0")]);
        assert_eq!(id.to_string(), "a@1.0.0(b@2.0.0(c@3.0.0))(d@4.0.0)");
    }

    #[test]
    fn a_long_peer_context_collapses_to_a_hash() {
        let peers: Vec<PackageId> = (0..40)
            .map(|n| plain(&format!("a-rather-long-peer-name-{n}"), "1.0.0"))
            .collect();
        let id = with_peers("p", "1.0.0", &peers);
        let rendered = id.to_string();

        assert!(
            rendered.len() <= MAX_KEY_BYTES,
            "a key must fit a path component, got {} bytes",
            rendered.len()
        );
        assert!(rendered.starts_with("p@1.0.0_"), "got {rendered}");
        assert!(
            !rendered.contains('('),
            "the suffix collapsed, got {rendered}"
        );
    }

    #[test]
    fn distinct_long_contexts_hash_differently() {
        let long = |extra: &str| {
            let mut peers: Vec<PackageId> = (0..40)
                .map(|n| plain(&format!("a-rather-long-peer-name-{n}"), "1.0.0"))
                .collect();
            peers.push(plain(extra, "1.0.0"));
            with_peers("p", "1.0.0", &peers).to_string()
        };
        assert_ne!(long("x"), long("y"));
    }

    #[test]
    fn a_key_splits_into_its_name_and_version() {
        assert_eq!(split_key("lodash@4.17.21"), Some(("lodash", "4.17.21")));
        assert_eq!(
            split_key("@types/node@20.0.0"),
            Some(("@types/node", "20.0.0")),
            "a scope's leading @ is not the separator"
        );
        assert_eq!(
            split_key("react-dom@18.2.0(react@18.2.0)"),
            Some(("react-dom", "18.2.0")),
            "the peer suffix is not part of the version"
        );
        assert_eq!(
            split_key("a@1.0.0(b@2.0.0(c@3.0.0))(d@4.0.0)"),
            Some(("a", "1.0.0")),
            "however deeply the suffix nests"
        );
        assert_eq!(
            split_key("p@1.0.0_0123456789abcdef"),
            Some(("p", "1.0.0")),
            "a collapsed suffix is still a suffix"
        );
        assert_eq!(split_key("nope"), None);
        assert_eq!(split_key("@scope/only"), None);
    }

    #[test]
    fn an_underscore_in_a_package_name_is_not_a_collapsed_suffix() {
        // npm permits `_` in a name, so the collapsed marker is recognised
        // only at the very end of the key and only as a fixed run of hex.
        // Anything laxer eats part of a legitimate name.
        assert_eq!(split_key("my_pkg@1.0.0"), Some(("my_pkg", "1.0.0")));
        assert_eq!(
            split_key("a_0123456789abcdef@1.0.0"),
            Some(("a_0123456789abcdef", "1.0.0")),
            "hex in the name, but the key does not end with it"
        );
        assert_eq!(
            split_key("my_pkg@1.0.0_0123456789abcdef"),
            Some(("my_pkg", "1.0.0")),
            "a real suffix on a name that also carries an underscore"
        );
        assert_eq!(
            split_key("p@1.0.0_nothex0123456"),
            Some(("p", "1.0.0_nothex0123456")),
            "not hex, so not a suffix — and the version keeps it"
        );
    }

    #[test]
    fn every_rendered_key_splits_back_to_its_name_and_version() {
        for id in [
            plain("lodash", "4.17.21"),
            plain("@types/node", "20.0.0"),
            with_peers("react-dom", "18.2.0", &[plain("react", "18.2.0")]),
            with_peers(
                "@storybook/react",
                "7.6.0",
                &[plain("@babel/core", "7.24.0"), plain("react", "18.2.0")],
            ),
            with_peers(
                "a",
                "1.0.0",
                &[with_peers("b", "2.0.0", &[plain("c", "3.0.0")])],
            ),
        ] {
            let rendered = id.to_string();
            assert_eq!(
                split_key(&rendered),
                Some((id.name.as_str(), id.version.as_str())),
                "{rendered} did not split back"
            );
        }
    }

    #[test]
    fn splitting_an_alias_target_is_unchanged_by_peers() {
        // `split_name_and_version` is shared with `alias_target`, which must
        // not learn about peers. It is left alone; `split_key` is the new one.
        assert_eq!(
            split_name_and_version("string-width@^4.0.0"),
            Some(("string-width", "^4.0.0"))
        );
        assert_eq!(
            alias_target("npm:safe-execa@0.3.0"),
            Some(("safe-execa", "0.3.0"))
        );
    }

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
