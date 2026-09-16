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
//! That the crawl holds a selected version's `dist.tarball` a whole
//! resolution before the walk reaches it is now *used* rather than merely
//! true: [`resolve_watching`] reports each one as it lands, and
//! `commands::install` downloads it while this module is still deciding
//! whether the tree wants it. The report is one-way by construction — a
//! [`Candidate`] goes out and nothing comes back — so the paragraph above
//! stands unchanged, and what a watcher does with one is somebody else's
//! question entirely.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use thiserror::Error;

use crate::binaries::Bins;
use crate::integrity::{Integrity, IntegrityError};
use crate::platform::{Platform, PlatformSupport};
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

/// Every dependency edge a version declares, and whether it is optional.
///
/// One function, used by both the crawl and the walk, because the two must ask
/// the registry about exactly the same set — the crawl's whole claim to
/// deciding nothing rests on it reading the same edges the walk will.
///
/// A name declared in *both* blocks appears once, as the optional one, which is
/// npm's documented rule: "entries in optionalDependencies will override
/// entries of the same name in dependencies". Written as a filter rather than
/// as an overwrite, so the rule holds for a caller that reads this as a
/// sequence as well as for one that collects it into a map — and so that the
/// walk does not resolve a version it is about to throw away.
fn declared_edges(metadata: &VersionMetadata) -> impl Iterator<Item = (&str, &str, bool)> {
    let required = metadata
        .dependencies
        .iter()
        .filter(|(name, _)| !metadata.optional_dependencies.contains_key(*name))
        .map(|(name, specifier)| (name.as_str(), specifier.as_str(), false));
    let optional = metadata
        .optional_dependencies
        .iter()
        .map(|(name, specifier)| (name.as_str(), specifier.as_str(), true));
    required.chain(optional)
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
    ///
    /// Every edge, optional or not. Which of them were declared optional is
    /// `optional_dependencies` below, and nothing else about an optional edge
    /// differs.
    pub dependencies: BTreeMap<String, PackageId>,
    /// Which keys of `dependencies` came from `optionalDependencies`.
    ///
    /// Spelled out rather than `optional`, because `DeclaredPeer::optional`
    /// already lives in this module and says something else entirely — that a
    /// peer may go unsatisfied. Two `.optional`s a few hundred lines apart,
    /// meaning different things, is a field name that has to be read twice.
    ///
    /// A marker over the edge names rather than a second edge map or a flag on
    /// the edge value, because an optional dependency that is installed is an
    /// ordinary dependency in every respect — it resolves, links, prunes and
    /// answers a peer identically. Optionality changes exactly one thing:
    /// whether a platform mismatch at the far end is tolerated. Keeping it out
    /// of the edge is what lets every existing walker over `dependencies` stay
    /// single-branch and stay right, and leaves the one pass that has the
    /// question — [`ResolvedGraph::for_platform`] — to ask it.
    pub optional_dependencies: BTreeSet<String>,
    /// What this package published about the machines it runs on.
    ///
    /// Carried rather than evaluated, for the reason `declared_peers` is: the
    /// answer depends on which machine is asking, the lockfile records this
    /// graph, and a lockfile that recorded one machine's answer would be a
    /// different file on every platform.
    pub supports: PlatformSupport,
    /// The CLI entry points this package publishes: the name each takes inside
    /// a `.bin` directory -> the file inside the package it points at.
    ///
    /// Carried for the reason `supports` and `declared_peers` are — it is a
    /// fact about the publish, and the lockfile has to record it so an install
    /// that resolves nothing still knows which shims to write. See
    /// `docs/specs/2026-09-16-bin-linking-design.md` §1.
    ///
    /// Already validated: a name that is not one path component and a target
    /// that leaves the package are dropped on the way in, by
    /// [`crate::binaries`], so nothing downstream has to ask.
    pub bins: Bins,
    /// What this package requires of its consumer, exactly as published.
    ///
    /// Carried rather than resolved by the walk, because a peer is not an edge
    /// to follow: it is a constraint answered by whoever installs this package,
    /// and the walk has no idea who that is. The peer pass does.
    ///
    /// Recorded even for packages whose peers all turn out to be satisfied, so
    /// that a diagnostic can be recomputed later from the graph alone.
    pub declared_peers: BTreeMap<String, DeclaredPeer>,
    /// What this node's own peers resolved to, by the name it declared them
    /// under. Empty until the peer pass has run, and empty for every package
    /// that declares none.
    ///
    /// Not derivable from `id.context`, which also holds the dependencies
    /// folded in to keep two copies apart — and a name can legitimately appear
    /// in both. The lockfile records what a package resolved *as a peer*, so
    /// the graph has to keep the two apart.
    pub peers: BTreeMap<String, PackageId>,
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
    /// A package declaring one of its own, and whether it declared it in
    /// `optionalDependencies`.
    ///
    /// The flag rides along rather than steering anything, exactly as `kind`
    /// does above: an optional edge is selected, fetched and recorded like any
    /// other, and the value is carried only so the graph can record which
    /// section asked.
    Package { id: PackageId, optional: bool },
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

    /// The sub-graph one machine actually materialises, and what it left out.
    ///
    /// A package declaring an `os` or `cpu` this machine does not satisfy is
    /// skipped — but *only* where it was reached through an optional edge,
    /// which is the only place a skip is available. Reached through a plain
    /// `dependencies` entry the constraint is not consulted at all: refusing a
    /// tree that works is not a stricter kind of correct, and `os` is advisory
    /// metadata publishers get wrong.
    ///
    /// Written as a walk that declines to *enter* a node rather than as a
    /// filter over the finished set, because that is what makes the subtree
    /// fall out for free. Anything only the skipped package led to is never
    /// reached, and anything it merely shared with a package that is kept is
    /// reached by the other path and stays — which is npm's rule, that a node
    /// is optional only when every path to it is, arrived at rather than
    /// stated.
    ///
    /// **This is not `reachable`, and must not be folded into it.** The
    /// lockfile is written from the full graph, which is what makes it
    /// platform independent: it records `@esbuild/win32-x64` on a Mac and on
    /// Linux alike, so the same committed file plans differently on each
    /// machine instead of being re-resolved into a different file on each. So
    /// the caller needs both graphs at once, which is why this borrows where
    /// `reachable` consumes.
    ///
    /// Two things happen beyond dropping nodes, both of them about not leaving
    /// a dangling link behind: an edge naming a dropped package is dropped
    /// from whoever declared it, and so is a resolved peer naming one. Without
    /// the first the plan writes a symlink into a virtual store entry nothing
    /// created.
    ///
    /// It clones, and in the common case where nothing is skipped it clones
    /// the whole graph to change nothing. A `Cow` would save that, and would
    /// put a deref in front of every reader of a graph for a cost that does
    /// not register beside one tarball — this runs once per install, against
    /// the fetch and the unpack that follow it.
    pub fn for_platform(&self, platform: &Platform) -> (Self, Vec<PackageId>) {
        let mut kept: BTreeSet<PackageId> = BTreeSet::new();
        let mut skipped: BTreeSet<PackageId> = BTreeSet::new();
        // An importer declares no optional section, so every edge leaving one
        // is required and is entered unconditionally.
        let mut queue: VecDeque<PackageId> = self
            .importers
            .values()
            .flat_map(|importer| importer.dependencies.values())
            .filter_map(|dependency| match &dependency.resolution {
                Resolution::Registry(id) => Some(id.clone()),
                Resolution::Local(_) => None,
            })
            .collect();

        while let Some(id) = queue.pop_front() {
            if !kept.insert(id.clone()) {
                continue;
            }
            let Some(package) = self.packages.get(&id) else {
                continue;
            };
            for (name, target) in &package.dependencies {
                let skippable = package.optional_dependencies.contains(name);
                let admitted = self
                    .packages
                    .get(target)
                    .is_none_or(|dependency| dependency.supports.admits(platform));
                if skippable && !admitted {
                    skipped.insert(target.clone());
                    continue;
                }
                queue.push_back(target.clone());
            }
        }

        // A node reached by one path and skipped on another is kept: the first
        // path is a reason to install it and the second is only permission not
        // to. Computed by subtraction rather than by checking at the point of
        // the skip, because the two paths can be discovered in either order.
        skipped.retain(|id| !kept.contains(id));

        let packages = self
            .packages
            .iter()
            .filter(|(id, _)| kept.contains(id))
            .map(|(id, package)| {
                let mut package = package.clone();
                package.dependencies.retain(|_, to| kept.contains(to));
                // The marker set names keys of `dependencies`, so it has to
                // follow them out. Nothing downstream reads it today, which is
                // exactly why leaving it stale would be a field whose own
                // documentation is false by the time something does.
                package
                    .optional_dependencies
                    .retain(|name| package.dependencies.contains_key(name));
                package.peers.retain(|_, to| kept.contains(to));
                (id.clone(), package)
            })
            .collect();

        (
            ResolvedGraph {
                importers: self.importers.clone(),
                packages,
            },
            skipped.into_iter().collect(),
        )
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

/// A version the crawl selected, and what it would take to download it.
///
/// Reported by [`resolve_watching`] the moment the packument naming it lands,
/// which is long before the walk reaches it — that head start is the whole
/// point of the type. It is **speculative**, in two senses that are worth
/// keeping apart. The crawl's ask set is order-dependent, so a candidate may
/// come from a version whose dependencies the resolved graph never contains.
/// And the graph is not the tree: a caller prunes it — for reachability, and
/// for the `os`/`cpu` of the machine it is installing onto — long after this
/// was reported, so a candidate may name a package that is resolved and then
/// never installed. Nothing here is a claim that the tree wants this package,
/// only that something asked about it.
///
/// The id is peer-blind — `context` is always empty — and that is not a
/// shortcoming. A tarball is a property of the published version, so every
/// peer duplicate of a node shares one, and the store is keyed by integrity
/// rather than by node. The id is here to name the package in an error, not to
/// find it in a graph.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: PackageId,
    /// The tarball URL, exactly as the packument gave it.
    pub resolved: String,
    pub integrity: Integrity,
    /// What the package published about the machines it runs on.
    ///
    /// Carried rather than evaluated, for the reason [`ResolvedPackage`]
    /// carries it: this module does not know which machine is asking, and a
    /// watcher that skips what its platform rules out is making a decision
    /// about its own bandwidth rather than about the graph.
    pub supports: PlatformSupport,
}

/// Told about each version the crawl selects, as it is selected.
///
/// `Sync` because the crawl calls it from every one of its workers, and there
/// is deliberately no way for it to answer: a watcher that could refuse a
/// candidate, or return an error, would be a second thing deciding what gets
/// resolved. It is told, and the walk goes on regardless.
pub type Watcher<'a> = &'a (dyn Fn(Candidate) + Sync);

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
    resolve_watching(registry, roots, members, &|_| {})
}

/// [`resolve`], reporting each version the crawl selects to `watch` as it is
/// selected.
///
/// The one thing the crawl knows that the walk does not know yet: which
/// tarballs this resolution is going to want. It learns each one a whole
/// resolution early — the moment a packument lands — and until now threw it
/// away. A caller that owns a content store can start downloading from here
/// instead of waiting for the barrier at the end of resolution, which is
/// [#95](https://github.com/jerky-build/jerky/issues/95).
///
/// **`watch` cannot affect the answer, and the type is what enforces it.** It
/// takes a [`Candidate`] and returns nothing, so there is no channel through
/// which what it does — or how long it takes, or whether it fails — can reach
/// a selection. Run this with a watcher, with a different watcher, or with
/// none, and the graph is the same bytes; that claim is the whole of why the
/// overlapped fetch does not have to reason about determinism at all.
///
/// The resolver still performs no I/O beyond the [`RegistryClient`], which is
/// the reason this reports candidates rather than fetching them. A store is a
/// filesystem, and the pass that decides what a tree contains has no business
/// knowing where one is.
pub fn resolve_watching(
    registry: &dyn RegistryClient,
    roots: &BTreeMap<ImporterPath, BTreeMap<String, Declared>>,
    members: &BTreeMap<String, ImporterPath>,
    watch: Watcher<'_>,
) -> Result<ResolvedGraph, ResolveError> {
    let mut walk = Walk::seeded(registry, roots, members)?;
    walk.prefetch(watch);
    walk.run()?;
    Ok(walk.into_graph())
}

/// A required peer that nothing in the dependent's environment satisfies.
///
/// Produced here and printed elsewhere: the resolver has no business writing to
/// a terminal, and the install command already returns its warnings for `main`
/// to render.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnsatisfiedPeer {
    /// The package that declared the peer, as published — no peer context, so
    /// one complaint reads the same however many copies of the package the
    /// duplication produced.
    pub dependent: PackageId,
    /// The name it declared, and the range it asked of it.
    pub peer: String,
    pub range: String,
    /// The version of the provider that was found, when one was found at all.
    ///
    /// `None` is "nothing provides this"; `Some` is "something does, and it is
    /// the wrong version". The two are different problems and read as different
    /// sentences, which is why this is an `Option` and not a bool saying it
    /// went wrong.
    pub found: Option<String>,
}

/// Resolve every package's peers, duplicating nodes whose answers differ.
///
/// A pure graph-to-graph pass. It fetches nothing and touches no filesystem,
/// which is what lets it be tested exactly the way the walk is — and it is
/// sound *only* because jerky never fabricates an edge: a peer is satisfied
/// from what is already resolved, so peer resolution can never cause a new
/// version to be selected, and can therefore run after selection is finished.
///
/// Takes the graph by value so no caller is left holding the peer-blind one,
/// which is a different graph with confusingly similar contents.
///
/// The diagnostics are read off the *finished* graph by
/// [`unsatisfied_peers`] rather than collected while resolving, so that the
/// complaint an install prints has one definition whether it resolved anything
/// or reloaded a lockfile — see that function for why the difference matters.
pub fn resolve_peers(graph: ResolvedGraph) -> (ResolvedGraph, Vec<UnsatisfiedPeer>) {
    let mut pass = PeerPass {
        source: &graph,
        needed: peer_names_needed(&graph),
        instances: BTreeMap::new(),
        identities: BTreeMap::new(),
        identifying: BTreeSet::new(),
        cutting: BTreeSet::new(),
    };

    // Stage one: discover which *instances* exist — one per (package, the
    // environment its subtree can see) — without giving any of them a name.
    let mut importers = graph.importers.clone();
    let mut roots = Vec::new();
    for (path, importer) in &importers {
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

        for (name, dependency) in &importer.dependencies {
            if let Resolution::Registry(id) = &dependency.resolution {
                let instance = pass.discover(id, &[&provided]);
                roots.push((path.clone(), name.clone(), instance));
            }
        }
    }

    // Stage two: name them. Separate from discovery because a node's name
    // depends on its dependencies' names, which is a question that cannot be
    // answered while still finding out what the dependencies are.
    for (.., instance) in &roots {
        pass.identify(instance);
    }

    // Stage three: emit. Every reachable instance has a name by now, so every
    // edge names a node that exists — including the edges that close a cycle,
    // which is the property the two-stage split buys.
    let packages = pass.emit();

    for (path, name, instance) in roots {
        let id = pass.identities[&instance].clone();
        if let Some(dependency) = importers
            .get_mut(&path)
            .and_then(|importer| importer.dependencies.get_mut(&name))
        {
            dependency.resolution = Resolution::Registry(id);
        }
    }

    let resolved = ResolvedGraph {
        importers,
        packages,
    };
    let unsatisfied = unsatisfied_peers(&resolved);

    (resolved, unsatisfied)
}

/// Every required peer a finished graph leaves unanswered.
///
/// A function of the graph alone, which is what makes a warning survive a
/// cache hit. An install whose importers all still match resolves nothing at
/// all, so there is no pass running to notice anything — the only thing left
/// to read is what the lockfile recorded, and `declared_peers` is on every
/// entry precisely so this can be recomputed from it. Install therefore asks
/// this of whatever graph it ends up with, re-resolved or reloaded, and gets
/// the same answer either way.
///
/// It is also why [`resolve_peers`] does not report as it goes, though it has
/// every answer to hand. Two producers of one complaint are two readings of
/// "unsatisfied" to keep in agreement forever, and the half nobody exercises
/// is the half that drifts. The rule itself lives in [`provider_of`] and
/// [`satisfies`], which both callers share; what this adds is the walk down.
pub fn unsatisfied_peers(graph: &ResolvedGraph) -> Vec<UnsatisfiedPeer> {
    let mut complaints = BTreeMap::new();
    let mut visited = BTreeSet::new();

    for importer in graph.importers.values() {
        // The importer's own dependencies are the outermost frame: the last
        // place a peer looks, and the only one a top-level package has. A
        // `Resolution::Local` is a workspace member, which is linked in place
        // and has no node to walk into.
        let provided: BTreeMap<String, PackageId> = importer
            .dependencies
            .iter()
            .filter_map(|(name, dependency)| match &dependency.resolution {
                Resolution::Registry(id) => Some((name.clone(), id.clone())),
                Resolution::Local(_) => None,
            })
            .collect();

        for id in provided.values() {
            complain(graph, id, &[&provided], &mut visited, &mut complaints);
        }
    }

    complaints.into_values().collect()
}

/// Walk one node and everything below it, complaining about peers as it goes.
///
/// `chain` runs outermost-first, so appending this node's own dependencies
/// makes "own dependencies, then nearest ancestor, then the importer" a walk
/// backwards through it — the same reading [`PeerPass::own_peers`] does over
/// the same two helpers.
///
/// `visited` is on the node's id rather than on the path that reached it,
/// which is what makes this terminate on a dependency cycle and is sound
/// because the id *is* the copy: a peer-resolved node carries what it resolved
/// against, so two routes to one key are two routes to one environment and
/// cannot disagree about who provides what.
///
/// Over a graph that was never peer-resolved — a lockfile an older jerky wrote
/// — the key is coarser than that, and a node two paths reach differently is
/// judged by whichever arrived first. The direction that errs in is silence,
/// which is the tolerable one here: such a file is regenerated the moment
/// anything about it is stale, and the install after that says everything.
fn complain<'a>(
    graph: &'a ResolvedGraph,
    id: &PackageId,
    chain: &[&'a BTreeMap<String, PackageId>],
    visited: &mut BTreeSet<PackageId>,
    complaints: &mut BTreeMap<(PackageId, String), UnsatisfiedPeer>,
) {
    // Nothing a walk produces, but a hand-written lockfile can name an edge it
    // does not record. Left to the lockfile's own validation to report.
    let Some(package) = graph.packages.get(id) else {
        return;
    };
    if !visited.insert(id.clone()) {
        return;
    }

    let mut chain: Vec<&BTreeMap<String, PackageId>> = chain.to_vec();
    chain.push(&package.dependencies);

    let dependent = published(id);
    for (name, declared) in &package.declared_peers {
        let found = match provider_of(name, &chain) {
            // The whole content of `optional`, and it is narrower than
            // "optional peers never warn": it says the peer may be *absent*,
            // not that any version of it will do, so the out-of-range arm
            // below does not check it.
            None if declared.optional => continue,
            None => None,
            Some((_, provider)) if satisfies(&provider.version, &declared.range) => continue,
            Some((_, provider)) => Some(provider.version),
        };

        complaints
            .entry((dependent.clone(), name.clone()))
            .or_insert_with(|| UnsatisfiedPeer {
                dependent: dependent.clone(),
                peer: name.clone(),
                range: declared.range.clone(),
                found,
            });
    }

    for dependency in package.dependencies.values() {
        complain(graph, dependency, &chain, visited, complaints);
    }
}

/// A node's id with its peer context dropped: the package as published.
///
/// What a complaint is keyed and named by, so that the §6 duplication does not
/// multiply one published fact by the number of copies it landed in. A reader
/// looking at `plugin@1.0.0 wants peer vue@^3.0.0` has a `package.json` to go
/// and read; `plugin@1.0.0(react@17.0.0)` names a directory instead, eleven
/// times over on a tree that duplicated eleven ways.
fn published(id: &PackageId) -> PackageId {
    PackageId {
        name: id.name.clone(),
        version: id.version.clone(),
        context: BTreeMap::new(),
    }
}

/// One copy of a package: the version the walk selected, plus the environment
/// its subtree can see.
///
/// The environment is what tells two copies apart, and it has to name every
/// peer the *subtree* can ask about rather than only the ones this package
/// declares. A package with no peers of its own is still duplicated by one
/// deeper down, so keying on its own peers would give it a single instance and
/// silently collapse the copies.
///
/// It names each provider as an *instance* rather than as the id the walk
/// selected, and that recursion is load-bearing rather than tidy. A provider
/// reached only across a peer edge contributes nothing to
/// [`peer_names_needed`], which follows dependency edges — so `host` peering
/// `mid`, and `mid` peering `leaf`, gives `host` the same peer-blind
/// `mid@1.0.0` under two importers supplying different `leaf`s, and the two
/// `host`s collapse into one wired to whichever `mid` was named first. Naming
/// the provider's copy closes that: the two `mid` copies differ, so the two
/// `host`s do. It closes the shadowed case with it, which widening the
/// alphabet alone would not — a provider's copy is settled by what was above
/// *the provider*, and an intermediate between it and the dependent that
/// happens to declare the same name cannot make two different providers look
/// alike.
///
/// Only names satisfied from *above* appear. A peer answered by the package's
/// own dependencies is answered identically in every copy of it — the
/// dependency ids are the walk's, and which copy of them answers follows from
/// this environment — so recording it would distinguish nothing. Leaving it
/// out is also half of what bounds the recursion: every entry is resolved
/// against a prefix of the chain no longer than the one that asked for it, so
/// the chain cannot grow as the lookups nest. The other half is the repeat
/// guard in [`PeerPass::instance_at`], since a prefix of the *same* length is
/// allowed and is what a provider in the immediately enclosing frame takes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Instance {
    package: PackageId,
    environment: BTreeMap<String, Instance>,
}

/// What one instance resolved, before anything has been named.
#[derive(Debug, Clone)]
struct Copy_ {
    /// This package's own peers, and the copy of each that answered.
    ///
    /// Instances rather than ids, for the reason `dependencies` are: a
    /// provider is named after the whole pass has run, and the name it ends up
    /// with need not be the one it was found under. `host` depends on `lib`
    /// and `lib` peers back on `host`: naming `lib` gives `host` a context of
    /// its own, at which point the plain `host@1.0.0` the walk answered with is
    /// no longer a node at all.
    own: BTreeMap<String, Instance>,
    /// What it depends on, as instances rather than ids.
    dependencies: BTreeMap<String, Instance>,
}

/// The peer pass's working state.
struct PeerPass<'a> {
    source: &'a ResolvedGraph,
    /// Every peer name each node's subtree can ask about, itself included.
    needed: BTreeMap<PackageId, BTreeSet<String>>,
    instances: BTreeMap<Instance, Copy_>,
    identities: BTreeMap<Instance, PackageId>,
    /// Instances whose name is still being computed. Re-entering one is a
    /// cycle, and the edge that closed it is left out of the name — see
    /// [`PeerPass::identify`].
    identifying: BTreeSet<Instance>,
    /// Instances whose *cut* name is being computed, which is a second and
    /// separate loop to break — see [`PeerPass::cut_name`].
    cutting: BTreeSet<Instance>,
}

impl PeerPass<'_> {
    /// Find every instance reachable from one edge, resolving peers as it goes.
    ///
    /// `providers` runs outermost-first: the importer, then each package on the
    /// path down, ending with this node's own dependencies.
    fn discover(&mut self, id: &PackageId, providers: &[&BTreeMap<String, PackageId>]) -> Instance {
        // Copied out so the borrow lives as long as the source graph rather
        // than as long as `&mut self`.
        let source = self.source;

        let instance = self.instance_of(id, providers);

        let Some(package) = source.packages.get(id) else {
            // Nothing a walk produces, but a hand-written lockfile can name an
            // edge it does not record. Recording the bare instance lets the
            // lockfile's own validation report that, rather than this pass
            // panicking on it first.
            self.instances.entry(instance.clone()).or_insert(Copy_ {
                own: BTreeMap::new(),
                dependencies: BTreeMap::new(),
            });
            return instance;
        };

        let mut chain: Vec<&BTreeMap<String, PackageId>> = providers.to_vec();
        chain.push(&package.dependencies);

        if self.instances.contains_key(&instance) {
            return instance;
        }
        // Recorded before descending, so a dependency cycle finds it present
        // and stops. Overwritten with the real answer below.
        self.instances.insert(
            instance.clone(),
            Copy_ {
                own: BTreeMap::new(),
                dependencies: BTreeMap::new(),
            },
        );

        let own = self.own_peers(package, &chain);
        let mut dependencies = BTreeMap::new();
        for (name, dependency) in &package.dependencies {
            dependencies.insert(name.clone(), self.discover(dependency, &chain));
        }

        self.instances
            .insert(instance.clone(), Copy_ { own, dependencies });

        instance
    }

    /// Give one instance its name, and every instance beneath it.
    ///
    /// The name is the package's own resolved peers plus each dependency that
    /// itself carries a context. That second half is what duplicates a package
    /// declaring no peers at all: without it two copies pointing at different
    /// subtrees would key the same, and `ResolvedGraph::packages` would keep
    /// one and drop the other's subtree with nothing reported.
    ///
    /// Returns `None` when the instance is already being named further up —
    /// the edge that closes a cycle. That edge is left out of the *name* and
    /// kept in `dependencies`, which is the only honest split available: a name
    /// is finite and owns its parts, so a cycle cannot be spelled out inside
    /// one, while the edge itself is real and must still point somewhere.
    fn identify(&mut self, instance: &Instance) -> Option<PackageId> {
        if let Some(id) = self.identities.get(instance) {
            return Some(id.clone());
        }
        if self.identifying.contains(instance) {
            return None;
        }
        self.identifying.insert(instance.clone());

        let copy = self.instances[instance].clone();

        // A peer contributes the name its provider ends up with, and falls
        // back to a *cut* name only when that name is the one being computed —
        // a peer pointing back up at an ancestor, where asking for the
        // provider's final name would be asking for this one.
        //
        // What is cut has to be the loop and not the provider. Two copies of a
        // dependent are told apart by which copy of the provider answered
        // them, so falling back to the peer-blind id the walk selected spells
        // both copies identically, and `emit` keeps one of them — the
        // duplication happening in the graph and not in the names it is
        // written under, which is #110 again through a different door.
        let mut own: BTreeMap<String, PackageId> = BTreeMap::new();
        for (name, provider) in &copy.own {
            let id = match self.identify(provider) {
                Some(id) => id,
                None => self.cut_name(provider),
            };
            own.insert(name.clone(), id);
        }
        let named: Vec<(String, PackageId)> = copy
            .dependencies
            .iter()
            .filter_map(|(name, dependency)| Some((name.clone(), self.identify(dependency)?)))
            .collect();

        let id = PackageId {
            name: instance.package.name.clone(),
            version: instance.package.version.clone(),
            context: context_of(own, named),
        };

        self.identifying.remove(instance);
        self.identities.insert(instance.clone(), id.clone());
        Some(id)
    }

    /// A provider's name, cut where it would ask for the name being computed.
    ///
    /// The same shape as [`Self::identify`] — own peers, plus each dependency
    /// that carries a context — over its own stack, so the only thing left out
    /// is the loop itself. An instance already named answers with that name,
    /// so a cut name appears in exactly one place: the suffix of a node whose
    /// peer points back up at an ancestor still being named.
    ///
    /// It can therefore differ from the name the provider is finally emitted
    /// under, and that is the established split rather than a new one. A name
    /// is finite and owns its parts, so a loop cannot be spelled out inside
    /// one; `peers` records the final id, which is the answer anything reading
    /// the graph wants, and the suffix records as much of the provider as can
    /// be written down. What matters is that it writes down enough: the cut is
    /// at the second visit to an instance rather than the first, so everything
    /// that distinguishes two copies of a provider short of the loop — its own
    /// resolved peers, and the contexts its dependencies carry — is in the
    /// name before anything is dropped.
    fn cut_name(&mut self, instance: &Instance) -> PackageId {
        if let Some(id) = self.identities.get(instance) {
            return id.clone();
        }
        // The second visit. `insert` reports whether it was the first, which
        // is the check and the mark in one call.
        if !self.cutting.insert(instance.clone()) {
            return instance.package.clone();
        }

        let Some(copy) = self.instances.get(instance).cloned() else {
            self.cutting.remove(instance);
            return instance.package.clone();
        };

        let mut own = BTreeMap::new();
        for (name, provider) in &copy.own {
            let id = self.cut_name(provider);
            own.insert(name.clone(), id);
        }

        let mut named = Vec::new();
        for (name, dependency) in &copy.dependencies {
            let id = self.cut_name(dependency);
            named.push((name.clone(), id));
        }

        self.cutting.remove(instance);

        PackageId {
            name: instance.package.name.clone(),
            version: instance.package.version.clone(),
            context: context_of(own, named),
        }
    }

    /// Build the re-keyed package map from the named instances.
    fn emit(&self) -> BTreeMap<PackageId, ResolvedPackage> {
        let mut packages = BTreeMap::new();

        for (instance, copy) in &self.instances {
            // An instance with no name was never reached from an importer.
            let Some(id) = self.identities.get(instance) else {
                continue;
            };
            let Some(source) = self.source.packages.get(&instance.package) else {
                continue;
            };

            let dependencies = copy
                .dependencies
                .iter()
                .filter_map(|(name, dependency)| {
                    Some((name.clone(), self.identities.get(dependency)?.clone()))
                })
                .collect();

            packages.insert(
                id.clone(),
                ResolvedPackage {
                    id: id.clone(),
                    resolved: source.resolved.clone(),
                    integrity: source.integrity.clone(),
                    dependencies,
                    // All three carried straight across. Peer duplication
                    // makes copies of one published version, and a copy
                    // declares what the version declared: which of its edges
                    // were optional, which machines it runs on, and which
                    // bins it ships are properties of the publish and not of
                    // which peers answered.
                    optional_dependencies: source.optional_dependencies.clone(),
                    supports: source.supports.clone(),
                    bins: source.bins.clone(),
                    declared_peers: source.declared_peers.clone(),
                    peers: copy
                        .own
                        .iter()
                        .map(|(name, provider)| {
                            // Named, necessarily: a provider is an ancestor's
                            // own dependency or an importer's, so whatever
                            // reached this node reached it too. Asserted
                            // rather than skipped, because dropping the edge
                            // while `id.context` still names the peer is the
                            // silent half of the bug this field just had.
                            let id = self.identities.get(provider).unwrap_or_else(|| {
                                panic!("peer {name} of {} was never named", instance.package)
                            });
                            (name.clone(), id.clone())
                        })
                        .collect(),
                },
            );
        }

        packages
    }

    /// Which provider answers each of this package's declared peers.
    ///
    /// The chain already has the package's own dependencies as its innermost
    /// frame, so "own dependencies first, then nearest ancestor, then the
    /// importer" is just walking it backwards. Own-first matters: a package
    /// declaring the same name in both `dependencies` and `peerDependencies` is
    /// saying "I will take yours, but I ship a fallback", and its own copy is
    /// the one it gets.
    ///
    /// Only what a peer *resolved to* is decided here. Whether an unanswered
    /// one is worth saying out loud is [`unsatisfied_peers`]'s question, asked
    /// of the graph this ends up building.
    fn own_peers(
        &self,
        package: &ResolvedPackage,
        chain: &[&BTreeMap<String, PackageId>],
    ) -> BTreeMap<String, Instance> {
        let mut own = BTreeMap::new();

        for (name, declared) in &package.declared_peers {
            let Some((frame, provider)) = provider_of(name, chain) else {
                continue;
            };

            // A provider outside the range is left out exactly as a missing
            // one is, optional or not, so the package links nothing rather
            // than linking a version it explicitly rejected.
            if satisfies(&provider.version, &declared.range) {
                // Truncated at the frame that named it, because that is what
                // was above the provider when the walk reached it. Slicing
                // here rather than passing the index down keeps the cut beside
                // the chain it cuts — the two are meaningless apart.
                own.insert(name.clone(), self.instance_of(&provider, &chain[..=frame]));
            }
        }

        own
    }

    /// Which copy of a package sits below `providers`.
    ///
    /// The one definition of what an instance *is*: the package, plus the copy
    /// of a provider for every peer name its subtree can ask about and cannot
    /// answer itself, looked up in what is visible from where it sits — the
    /// frames above it, then its own dependencies. [`Self::discover`] asks this
    /// of the node it is walking into; [`Self::own_peers`] asks it of a
    /// provider it found partway up the chain, handing over only the frames
    /// that were above *that*. A second spelling of this would be a second
    /// answer to "are these the same copy", which is the question the whole
    /// pass turns on.
    fn instance_of(&self, id: &PackageId, providers: &[&BTreeMap<String, PackageId>]) -> Instance {
        self.instance_at(id, providers, &mut BTreeSet::new())
    }

    /// [`Self::instance_of`], carrying the queries already on the stack.
    ///
    /// An environment names provider *copies*, so answering one query asks
    /// another, and a peer pointing back up can ask the question that is
    /// already being answered: an importer supplying `a` while `a`'s subtree
    /// peers `a`. `computing` holds `(package, how much of the chain was
    /// visible)`, which identifies a query exactly — every nested lookup takes
    /// a prefix of the same chain, so its length names the prefix — and
    /// re-entering one yields the package with no environment of its own.
    ///
    /// That is the cycle rule the rendered name already lives under: a name is
    /// finite and owns its parts, so a loop cannot be spelled out inside one.
    /// The truncation is the same kind of under-fragmentation, and in the same
    /// direction — a copy told apart from fewer things than it might be, never
    /// two copies conflated that the loop itself distinguishes.
    fn instance_at(
        &self,
        id: &PackageId,
        providers: &[&BTreeMap<String, PackageId>],
        computing: &mut BTreeSet<(PackageId, usize)>,
    ) -> Instance {
        let bare = || Instance {
            package: id.clone(),
            environment: BTreeMap::new(),
        };

        let Some(package) = self.source.packages.get(id) else {
            return bare();
        };

        let query = (id.clone(), providers.len());
        if computing.contains(&query) {
            return bare();
        }

        let mut visible: Vec<&BTreeMap<String, PackageId>> = providers.to_vec();
        visible.push(&package.dependencies);
        // The frame the package itself contributes. A name answered there is
        // answered the same way in every copy, which is why it is left out.
        let own_frame = providers.len();

        computing.insert(query.clone());
        let mut environment = BTreeMap::new();
        for name in self.needed.get(id).into_iter().flatten() {
            let Some((frame, found)) = provider_of(name, &visible) else {
                continue;
            };
            if frame == own_frame {
                continue;
            }
            // Truncated at the frame that named it, for the reason
            // [`Self::own_peers`] truncates: that is what was above the
            // provider, and it is what settles which copy of it this is.
            let provider = self.instance_at(&found, &visible[..=frame], computing);
            environment.insert(name.clone(), provider);
        }
        computing.remove(&query);

        Instance {
            package: id.clone(),
            environment,
        }
    }
}

/// What names a node: its own resolved peers, plus every dependency that
/// itself carries a context.
///
/// The second half is the one easy to leave out, and leaving it out is silent:
/// a package declaring no peers at all is still duplicated by one deeper down,
/// so two copies pointing at different subtrees would key identically and
/// `ResolvedGraph::packages` would keep one and drop the other's whole subtree.
/// A dependency's entry wins over a peer of the same name, which is the order
/// [`PeerPass::own_peers`] already resolves in — a package shipping a fallback
/// for a peer it would rather take from above gets its own copy.
///
/// Shared with the lockfile deliberately. This is a *rule* with two readers —
/// the pass that names a node and the load that rebuilds the name from what
/// was recorded — and the two must agree forever, or a node is written back
/// under a key it was never read from and every install renames directories
/// nothing asked it to touch. One spelling is how that is guaranteed rather
/// than remembered, which is the argument `split_name_and_version` already
/// makes about having one decoder for one encoder.
pub(crate) fn context_of(
    own: BTreeMap<String, PackageId>,
    dependencies: impl IntoIterator<Item = (String, PackageId)>,
) -> BTreeMap<String, PackageId> {
    let mut context = own;
    for (name, id) in dependencies {
        if !id.context.is_empty() {
            context.insert(name, id);
        }
    }
    context
}

/// Find who provides `name`, nearest frame first, and which frame that was.
///
/// `chain` runs outermost-first — importer, then each package down the path —
/// so walking it in reverse walks back up the tree from nearest to furthest.
///
/// The frame is returned because the id alone does not say *which copy* of the
/// provider answered, and a provider can exist several times over.
/// [`PeerPass::instance_at`] is what turns the pair back into one.
fn provider_of(name: &str, chain: &[&BTreeMap<String, PackageId>]) -> Option<(usize, PackageId)> {
    chain
        .iter()
        .enumerate()
        .rev()
        .find_map(|(frame, provided)| provided.get(name).map(|id| (frame, id.clone())))
}

/// Every peer name each node's subtree can ask about along a dependency edge,
/// itself included.
///
/// Half of what an instance is keyed on, and the reason it has to exist: a
/// package declaring no peers of its own is still duplicated by one deeper
/// down, so "what did *this* node resolve" cannot tell two copies apart. What
/// separates them is what the subtree beneath them resolved, and this names the
/// part of the environment a dependency edge can reach.
///
/// Only that part, and the limit is deliberate rather than a gap left open. A
/// provider reached across a *peer* edge is not in any of these sets — `host`
/// peering `mid` never gets `mid`'s own `leaf` — and it does not need to be,
/// because [`Instance`] names the provider's copy and that copy carries what
/// its own subtree asked for. Growing the alphabet across peer edges instead
/// would be both looser and wrong: looser because it names the alphabet by
/// package name rather than by which copy answered, wrong because it looks
/// every name up where the *dependent* sits, and a nearer package of that name
/// answers there while the peer was answered further up.
///
/// A least fixed point rather than a recursive walk, because the dependency
/// graph has cycles. The sets only grow over a finite alphabet, so it settles.
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
/// An unparseable range fails closed, and is reported as unsatisfied. A peer
/// range is whatever the publisher typed, and treating nonsense as satisfied
/// would silently record a peer resolution nobody asked for.
fn satisfies(version: &str, range: &str) -> bool {
    match (Version::parse(version), Range::parse(range)) {
        (Ok(version), Ok(range)) => range.matches(&version),
        _ => false,
    }
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
    fn prefetch(&self, watch: Watcher<'_>) {
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

            // Reported before the edges below are scheduled, so that the
            // first thing to happen after a packument lands is the download
            // it makes possible. A version whose `dist` carries no usable
            // hash is simply not reported: there is nothing to verify an
            // answer against, and the walk reaches the same version in its own
            // order and raises the error there.
            if let Ok(integrity) = metadata.dist.integrity() {
                watch(Candidate {
                    id: PackageId::plain(asked.name.clone(), version.clone()),
                    resolved: metadata.dist.tarball.clone(),
                    integrity,
                    supports: PlatformSupport {
                        os: metadata.os.clone(),
                        cpu: metadata.cpu.clone(),
                    },
                });
            }

            // Exactly the edges `visit` reads, through the same function: a
            // dependency's `devDependencies` must never be followed, and
            // `VersionMetadata` has no field they could arrive through. An
            // optional dependency is crawled like any other — whether this
            // machine ends up linking it is settled long after resolution, and
            // a crawl that guessed would be deciding something.
            for (name, specifier, _) in declared_edges(metadata) {
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
            Dependent::Package {
                id: parent,
                optional,
            } => {
                if let Some(package) = self.packages.get_mut(&parent) {
                    package.dependencies.insert(name.clone(), id.clone());
                    if optional {
                        package.optional_dependencies.insert(name.clone());
                    }
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

        // `dependencies` and `optionalDependencies`. Still no
        // `devDependencies`: a dependency's must never be followed — doing so
        // pulls in most of the registry — which is why `VersionMetadata` has
        // no field for them to be read from.
        //
        // A name in both blocks arrives once, as the optional one at the
        // optional range, which is npm's rule and `declared_edges`'s job.
        for (dep_name, dep_specifier, optional) in declared_edges(metadata) {
            self.work.push_back(Pending {
                dependent: Dependent::Package {
                    id: id.clone(),
                    optional,
                },
                name: dep_name.to_string(),
                specifier: dep_specifier.to_string(),
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
                // Filled as each edge above is visited, since that is where
                // the name an edge is recorded under is settled.
                optional_dependencies: BTreeSet::new(),
                supports: PlatformSupport {
                    os: metadata.os.clone(),
                    cpu: metadata.cpu.clone(),
                },
                bins: metadata.bins(),
                declared_peers: declared_peers(metadata),
                // Filled by the peer pass. The walk is peer-blind by design.
                peers: BTreeMap::new(),
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
