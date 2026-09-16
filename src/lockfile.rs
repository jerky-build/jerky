//! `jerky-lock.json`: the resolved graph, written down.
//!
//! The on-disk shape is a separate type from [`ResolvedGraph`] on purpose. The
//! graph is whatever the resolver finds convenient; the lockfile is what is
//! stable to commit. Conflating them would make every internal refactor a
//! format change, and this is a file real projects commit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::integrity::Integrity;
use crate::resolver::{
    DeclaredPeer, Dependency, Importer, ImporterPath, Kind, PackageId, Resolution, ResolvedGraph,
    ResolvedPackage, context_of, split_key,
};

pub const LOCKFILE_NAME: &str = "jerky-lock.json";

/// Bumped when the on-disk shape changes.
///
/// Present from the first release because a committed format without one has
/// no migration path: there is no way to tell an old file from a corrupt one.
///
/// Deliberately still `1` after peers were added, and after the shape change
/// before that. jerky has not shipped a 1.0, so there is no population of
/// committed lockfiles for a version number to tell apart — the format simply
/// changes, and a stale file is regenerated. Spending the version now would
/// buy nothing and leave a number in the file's history that never
/// distinguished anything.
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
    #[error("{path} keys `{entry}` but what that entry records identifies `{rebuilt}`")]
    KeyIdentityMismatch {
        path: PathBuf,
        entry: String,
        rebuilt: String,
    },
    #[error("{path} has `{entry}` depending on `{dependency}`, which it does not record")]
    DanglingEdge {
        path: PathBuf,
        entry: String,
        dependency: String,
    },
    #[error(
        "{path} has importer `{importer}` linking `{name}` to `{target}`, which leaves the workspace"
    )]
    LinkEscapesWorkspace {
        path: PathBuf,
        importer: String,
        name: String,
        target: String,
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

/// One importer's two sections, which is pnpm's shape.
///
/// The graph keeps one flat map with the kind as a field; the split happens
/// here and nowhere else. Recording it at all is what makes a dependency
/// moving between sections a real diff rather than nothing, and what lets a
/// production install be planned from this file alone.
///
/// Both blocks are skipped when empty, so a project with no devDependencies
/// grows no empty object and the common case reads exactly as it did before.
#[derive(Serialize, Deserialize, Default)]
struct OnDiskImporter {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    dependencies: BTreeMap<String, OnDiskDependency>,
    #[serde(
        rename = "devDependencies",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    dev_dependencies: BTreeMap<String, OnDiskDependency>,
}

#[derive(Serialize, Deserialize)]
struct OnDiskDependency {
    /// What the manifest asked for, verbatim.
    specifier: String,
    /// What it resolved to, in one of three forms:
    ///
    /// - `4.17.21` — a registry package whose name matches the key it is
    ///   declared under, which is the overwhelmingly common case.
    /// - `safe-execa@0.3.0` — a registry package whose name *differs* from the
    ///   key, i.e. an alias. Without the name the package would be
    ///   unidentifiable, since the key is the local name rather than the real
    ///   one. This is pnpm's encoding, taken from its own lockfile.
    /// - `link:../../packages/ui` — a workspace member, linked in place.
    ///
    /// The spelling exists only here; above this boundary it is a
    /// `Resolution`, so consumers cannot forget a case.
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
    /// What this node's own peers resolved to: the name it declared each under
    /// -> the key of the node that answered.
    ///
    /// A block of its own rather than extra `dependencies` entries. Merging
    /// them would leave both the linker and this format untouched, which is
    /// exactly what makes it tempting, and it would make the recorded
    /// `dependencies` stop corresponding to what the package published —
    /// anyone diffing this file against a real `package.json` would find edges
    /// the package never declared. Provenance is also what a `--strict-peers`,
    /// or a `why`, has to read.
    ///
    /// The whole key, never abbreviated the way a dependency edge is. A peer
    /// is looked up by the name the *dependent* uses, so an alias makes the
    /// provider's own name a different one, and there is no case common enough
    /// to be worth a second spelling here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    peers: BTreeMap<String, String>,
    /// What the package published, range and optional flag both.
    ///
    /// Recorded because a warning has to survive a cache hit. An install whose
    /// importers all still match resolves nothing at all, so a project would
    /// warn about an unsatisfied peer on the first install and go quiet on
    /// every one after it, with nothing about the project having changed. With
    /// the declared ranges on disk the warning is a pure function of this file.
    ///
    /// Recording the *diagnostics* instead would be simpler and wrong: derived
    /// state in a file whose job is recording facts, and stale the moment a
    /// manifest is edited by hand.
    #[serde(
        rename = "declaredPeers",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    declared_peers: BTreeMap<String, OnDiskDeclaredPeer>,
}

/// One published `peerDependencies` entry, as the file records it.
///
/// Its own type for the reason the whole module has one: the resolver's
/// [`DeclaredPeer`] is free to grow whatever the peer pass finds convenient,
/// and none of that should reach a committed file by default.
#[derive(Serialize, Deserialize)]
struct OnDiskDeclaredPeer {
    range: String,
    /// Written only when true, which keeps the common peer a single line. An
    /// absent flag reads as `false`, which is what `peerDependenciesMeta`
    /// saying nothing about a peer already means.
    #[serde(default, skip_serializing_if = "is_not_optional")]
    optional: bool,
}

fn is_not_optional(optional: &bool) -> bool {
    !optional
}

/// Does a `link:` target, read relative to its importer, stay inside the
/// workspace?
///
/// Walked rather than canonicalized because the directories need not exist
/// yet: a lockfile is read before anything is linked.
fn stays_within_workspace(importer: &ImporterPath, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }

    let mut depth = importer.depth() as isize;
    for component in target.components() {
        match component {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::CurDir => {}
            // A prefix or root component means it was not relative after all.
            _ => return false,
        }
    }
    true
}

/// How an edge names the node it points at, in both an importer's block and a
/// package's own.
///
/// The target's key with `name@` left off when that name is the one the edge
/// is already declared under — the overwhelmingly common case, and what keeps
/// an ordinary edge the bare `1.0.0` it has always been. An alias keeps the
/// name: the edge is recorded under the *local* name, so `execa` resolved to
/// `safe-execa@0.3.0` is unidentifiable without it. That spelling is pnpm's,
/// taken from its own lockfile.
///
/// What is never left off is the peer suffix. Writing `id.version` was enough
/// while every key was `name@version` and stopped being enough the moment two
/// peer resolutions of one version could both exist: the edge would name a
/// `plugin@1.0.0` the file does not record, and `load` would report it as a
/// dangling edge — jerky writing a lockfile jerky cannot read.
fn edge_value(name: &str, id: &PackageId) -> String {
    if id.name == name {
        // A rendered key opens with the package's own name, so dropping it is
        // a trim rather than a second rendering with a part left out.
        id.to_string()[name.len() + 1..].to_string()
    } else {
        id.to_string()
    }
}

/// The key an edge value names: the inverse of [`edge_value`].
///
/// A bare version carries no `@`, so anything that splits as `name@version` —
/// suffix and all — is naming a package of its own, and anything that does not
/// is naming the one the edge is declared under. One decoder for both halves
/// of the file, because two would be two chances to disagree.
fn edge_key(name: &str, recorded: &str) -> String {
    match split_key(recorded) {
        Some(_) => recorded.to_string(),
        None => format!("{name}@{recorded}"),
    }
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
                        .map(|(name, id)| (name.clone(), edge_value(name, id)))
                        .collect(),
                    peers: package
                        .peers
                        .iter()
                        .map(|(name, id)| (name.clone(), id.to_string()))
                        .collect(),
                    declared_peers: package
                        .declared_peers
                        .iter()
                        .map(|(name, declared)| {
                            (
                                name.clone(),
                                OnDiskDeclaredPeer {
                                    range: declared.range.clone(),
                                    optional: declared.optional,
                                },
                            )
                        })
                        .collect(),
                },
            )
        })
        .collect();

    let importers = graph
        .importers
        .iter()
        .map(|(path, importer)| {
            // Partitioned on the way out, which is the only place the two
            // sections exist as separate things.
            let mut on_disk = OnDiskImporter::default();
            for (name, dependency) in &importer.dependencies {
                let block = match dependency.kind {
                    Kind::Prod => &mut on_disk.dependencies,
                    Kind::Dev => &mut on_disk.dev_dependencies,
                };
                block.insert(
                    name.clone(),
                    OnDiskDependency {
                        specifier: dependency.specifier.clone(),
                        version: match &dependency.resolution {
                            Resolution::Registry(id) => edge_value(name, id),
                            Resolution::Local(target) => {
                                format!("{LINK_PREFIX}{}", target.display())
                            }
                        },
                    },
                );
            }
            (path.as_str().to_string(), on_disk)
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

/// What reading an entry's key and hash settled, so nothing below has to ask
/// again whether either is well-formed.
struct DecodedEntry {
    name: String,
    integrity: Integrity,
}

/// Rebuild every node's [`PackageId`] from what its entry records, rather than
/// from the key it is recorded under.
///
/// This is the whole reason the two are recorded separately rather than one
/// being derived from the other.
/// A key carries the peer context too, but reading it back out is not
/// available: a context long enough to collapse is a hash, and a hash does not
/// decode. Version 1 built a [`PackageId::plain`] from the key and dropped
/// whatever followed, which was sound only because nothing produced a suffix —
/// the moment one is real, two keys differing only in context parse to one id
/// and collapse onto one node, dropping a whole subtree with nothing reported.
///
/// So the identity is rebuilt exactly the way the peer pass built it: this
/// node's own resolved peers, plus every dependency that itself carries a
/// context. Both halves are recorded — the first in `peers`, the second
/// reachable through `dependencies` — and the second is what keeps two copies
/// of a package declaring no peers at all apart.
struct Identities<'a> {
    entries: &'a BTreeMap<String, Entry>,
    decoded: &'a BTreeMap<String, DecodedEntry>,
    named: BTreeMap<String, PackageId>,
    /// Keys whose identity is still being built. Re-entering one is a
    /// dependency cycle, and the edge that closed it contributes no context —
    /// the same split the peer pass makes, and the only honest one available:
    /// a context owns its values, so it is a finite tree and a cycle cannot be
    /// spelled inside one.
    naming: BTreeSet<String>,
}

impl<'a> Identities<'a> {
    /// Every recorded key, named.
    ///
    /// Walked from the importers first, in the order the peer pass named them,
    /// and only then swept for whatever is left. The order is load-bearing
    /// wherever a cycle is: a cycle has to be broken somewhere, the pass breaks
    /// it at whichever edge closes the loop it entered, and entering the loop
    /// from elsewhere breaks a different edge and gives a node a context the
    /// machine that wrote the file never gave it. That node would then be
    /// written back under a key it was not read from — a lockfile whose keys
    /// move on an install that changed nothing, and virtual store directories
    /// renamed along with them.
    ///
    /// The sweep still has work to do: a hand-edited file may record a package
    /// no importer reaches, and it is named rather than quietly skipped.
    fn rebuild(
        entries: &'a BTreeMap<String, Entry>,
        decoded: &'a BTreeMap<String, DecodedEntry>,
        importers: &BTreeMap<String, OnDiskImporter>,
    ) -> BTreeMap<String, PackageId> {
        let mut identities = Identities {
            entries,
            decoded,
            named: BTreeMap::new(),
            naming: BTreeSet::new(),
        };

        for importer in importers.values() {
            // Flattened back into the one map the graph keeps, because that is
            // the map the pass saw: the two blocks are an on-disk split and
            // have no say in what order anything was named.
            let mut roots: BTreeMap<&String, &String> = BTreeMap::new();
            for (name, dependency) in importer
                .dev_dependencies
                .iter()
                .chain(&importer.dependencies)
            {
                roots.insert(name, &dependency.version);
            }

            for (name, recorded) in roots {
                // A `link:` target and an edge naming nothing both land here
                // and are both left alone: this is the order a walk takes, and
                // the file's own validation is what reports them.
                let key = edge_key(name, recorded);
                if entries.contains_key(&key) {
                    identities.of(&key);
                }
            }
        }

        for key in entries.keys() {
            identities.of(key);
        }
        identities.named
    }

    /// The identity of one recorded key.
    ///
    /// Every key reached from here is one the dangling-edge check has already
    /// found in the file, which is what lets this index rather than ask.
    fn of(&mut self, key: &str) -> PackageId {
        if let Some(id) = self.named.get(key) {
            return id.clone();
        }

        // Copied out so the borrows live as long as the file's own maps rather
        // than as long as `&mut self`.
        let (entries, decoded) = (self.entries, self.decoded);
        let entry = &entries[key];
        let name = decoded[key].name.clone();

        if !self.naming.insert(key.to_string()) {
            // The cycle-closing edge. Deliberately not remembered: this is a
            // partial answer for one caller, not this key's identity.
            return PackageId::plain(name, entry.version.clone());
        }

        let mut own = BTreeMap::new();
        for (declared, provider) in &entry.peers {
            own.insert(declared.clone(), self.of(provider));
        }
        let mut dependencies = Vec::with_capacity(entry.dependencies.len());
        for (declared, recorded) in &entry.dependencies {
            dependencies.push((declared.clone(), self.of(&edge_key(declared, recorded))));
        }

        let id = PackageId {
            name,
            version: entry.version.clone(),
            // The fold itself lives in the resolver, beside the pass that
            // named these nodes in the first place. Spelling it again here is
            // what would let the two drift.
            context: context_of(own, dependencies),
        };
        self.naming.remove(key);
        self.named.insert(key.to_string(), id.clone());
        id
    }
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

    let entries = on_disk.packages;

    // Every key decoded once, before a single edge is followed. Nothing below
    // this loop asks whether a key is well-formed again.
    let mut decoded: BTreeMap<String, DecodedEntry> = BTreeMap::new();
    for (key, entry) in &entries {
        // `split_key`, not `split_name_and_version`: this is a *key*, and a
        // key may carry a peer suffix. What the suffix says is not read back
        // from it — see [`Identities`] — so all that is wanted here is where
        // the name ends.
        let (name, keyed_version) = split_key(key).ok_or_else(|| LockfileError::BadKey {
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

        decoded.insert(
            key.clone(),
            DecodedEntry {
                name: name.to_string(),
                integrity,
            },
        );
    }

    // Edges are checked only once every key is known, since a dependency may
    // be recorded after its dependent. An edge pointing at nothing means the
    // graph cannot be installed, and saying so beats discovering it halfway
    // through linking. Checked here, on the keys themselves, rather than after
    // the identities are built: an identity is *rebuilt from* the edges, so an
    // edge naming nothing has to be caught before anything reads it.
    //
    // A peer is an edge like any other. It is linked like one, so a peer
    // naming a node the file does not record is the same broken install a
    // dangling dependency is.
    for (key, entry) in &entries {
        let dependencies = entry
            .dependencies
            .iter()
            .map(|(name, recorded)| edge_key(name, recorded));
        for target in dependencies.chain(entry.peers.values().cloned()) {
            if !entries.contains_key(&target) {
                return Err(LockfileError::DanglingEdge {
                    path,
                    entry: key.clone(),
                    dependency: target,
                });
            }
        }
    }

    let ids = Identities::rebuild(&entries, &decoded, &on_disk.importers);

    // The key states the peer context and so do `peers` and `dependencies`, so
    // the two can disagree — the same shape of problem `KeyVersionMismatch`
    // above refuses, one field along. Refused rather than arbitrated, and the
    // key is not the half to trust: a collapsed suffix is a hash and says
    // nothing a reader can act on, which is why identity is rebuilt from the
    // records in the first place.
    //
    // It also settles the collapse this rebuild exists to prevent, and settles
    // it by construction: every key renders its own identity, so two keys
    // cannot name one node without one of them first being caught here.
    for (key, id) in &ids {
        let rebuilt = id.to_string();
        if &rebuilt != key {
            return Err(LockfileError::KeyIdentityMismatch {
                path,
                entry: key.clone(),
                rebuilt,
            });
        }
    }

    let mut packages = BTreeMap::new();
    for (key, entry) in entries {
        let id = ids[&key].clone();
        let DecodedEntry { integrity, .. } =
            decoded.remove(&key).expect("every key was decoded above");

        packages.insert(
            id.clone(),
            ResolvedPackage {
                id,
                resolved: entry.resolved,
                integrity,
                dependencies: entry
                    .dependencies
                    .iter()
                    .map(|(name, recorded)| (name.clone(), ids[&edge_key(name, recorded)].clone()))
                    .collect(),
                peers: entry
                    .peers
                    .iter()
                    .map(|(name, target)| (name.clone(), ids[target].clone()))
                    .collect(),
                declared_peers: entry
                    .declared_peers
                    .into_iter()
                    .map(|(name, declared)| {
                        (
                            name,
                            DeclaredPeer {
                                range: declared.range,
                                optional: declared.optional,
                            },
                        )
                    })
                    .collect(),
            },
        );
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

        // Read back into one flat map, because that is what the graph is.
        // `dependencies` is read second so that a hand-edited file naming one
        // package in both blocks collapses the way a manifest declaring it
        // twice does — to the production entry — rather than a different way.
        let mut dependencies = BTreeMap::new();
        let blocks = [
            (Kind::Dev, on_disk_importer.dev_dependencies),
            (Kind::Prod, on_disk_importer.dependencies),
        ];
        for (name, dependency, kind) in blocks
            .into_iter()
            .flat_map(|(kind, block)| block.into_iter().map(move |(n, d)| (n, d, kind)))
        {
            let resolution = match dependency.version.strip_prefix(LINK_PREFIX) {
                Some(target) => {
                    // A link legitimately climbs out of the importer — that is
                    // how `apps/web` reaches `packages/ui` — but it must not
                    // climb out of the *workspace*. Untrusted file, same
                    // escape class as the importer key beside it.
                    let target = PathBuf::from(target);
                    if !stays_within_workspace(&importer_path, &target) {
                        return Err(LockfileError::LinkEscapesWorkspace {
                            path,
                            importer: raw.clone(),
                            name: name.clone(),
                            target: dependency.version.clone(),
                        });
                    }
                    Resolution::Local(target)
                }
                None => {
                    // An importer's edge is spelled exactly as a package's, so
                    // it is decoded by the same function, and the node it
                    // names is looked up by key rather than rebuilt: the key
                    // is what the file records and the identity behind it is
                    // already settled above.
                    let Some(id) = ids.get(&edge_key(&name, &dependency.version)) else {
                        return Err(LockfileError::UnknownImporterDependency {
                            path: path.clone(),
                            importer: raw.clone(),
                            name: name.clone(),
                            version: dependency.version.clone(),
                        });
                    };
                    Resolution::Registry(id.clone())
                }
            };

            dependencies.insert(
                name,
                Dependency {
                    specifier: dependency.specifier,
                    kind,
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
