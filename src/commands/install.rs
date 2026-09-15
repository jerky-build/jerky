use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use thiserror::Error;

use crate::archive::{self, ArchiveError};
use crate::cli::{PackageSpec, VersionSpec};
use crate::integrity::{Integrity, IntegrityError};
use crate::linker::{self, LinkError, Unowned};
use crate::lockfile::{self, LockfileError};
use crate::manifest::{Manifest, ManifestError};
use crate::pool;
use crate::range::{Range, Version};
use crate::registry::{MAX_CONCURRENT_FETCHES, RegistryClient, RegistryError};
use crate::resolver::{
    self, ALIAS_PROTOCOL, Declared, Importer, ImporterPath, Kind, PackageId, Resolution,
    ResolveError, ResolvedGraph, ResolvedPackage,
};
use crate::store::{Store, StoreError};
use crate::workspace::Workspace;

#[derive(Debug, Error)]
pub enum InstallError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Lockfile(#[from] LockfileError),
    #[error("integrity check failed for {name}@{version}")]
    Integrity {
        name: String,
        version: String,
        #[source]
        source: IntegrityError,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Link(#[from] LinkError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(
        "the lockfile records integrity `{locked}` for `{name}@{version}`, but the registry now \
         reports `{reported}`. Refusing to install: a republished or tampered tarball for an \
         already-pinned version is exactly what the lockfile exists to catch."
    )]
    LockedIntegrityMismatch {
        name: String,
        version: String,
        locked: String,
        reported: String,
    },
    #[error(
        "`{name}@{requested}` constrains nothing — it accepts every version of \
         `{name}` the registry has today and every one it publishes later. Ask \
         for the range you mean (`^4.0.0`, `~4.17.0`), or `jerky install {name}` \
         to pin whatever `latest` resolves to now."
    )]
    UnconstrainedRange { name: String, requested: String },
    #[error("`{importer}` is not a member of this workspace (members: {})", members.join(", "))]
    UnknownImporter {
        importer: String,
        members: Vec<String>,
    },
    #[error(
        "`--production` installs what `{}` records, and there is no lockfile at that path. \
         Run `jerky install` first and commit the lockfile it writes.",
        path.display()
    )]
    ProductionLockfileMissing { path: PathBuf },
    #[error(
        "the lockfile does not match `{importer}`: `{name}` is declared as {declared} but the \
         lockfile records {locked}. `--production` installs only what the lockfile already \
         describes, so it refuses a manifest edited without reinstalling — including a \
         `devDependencies` edit, which would otherwise let CI pass on a lockfile that is \
         genuinely out of date. Run `jerky install` and commit the updated lockfile."
    )]
    ProductionLockfileStale {
        importer: String,
        name: String,
        declared: String,
        locked: String,
    },
    #[error(
        "the lockfile records `{importer}`, which is no longer a member of this workspace. \
         `--production` installs only what the lockfile already describes, and this one \
         describes a project that is gone — `jerky install` would rewrite it. Run it and \
         commit the updated lockfile."
    )]
    ProductionLockfileImporterGone { importer: String },
}

/// What a sync is for.
///
/// The two modes differ in what they are *allowed* to do, not in how much of
/// the same work they perform, which is why this is a parameter to `sync`
/// rather than a second function beside it: linking, convergence and pruning
/// are identical either way, and a copy of them would drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Every kind, every importer. Resolves what is stale and writes the
    /// lockfile.
    #[default]
    Develop,
    /// `dependencies` only, and a lockfile that must already match every
    /// manifest. Never writes one.
    ///
    /// Requiring the match is what *earns* the read-only property rather than
    /// enforcing it as a special case: with every importer reusable there is
    /// nothing to resolve, so there is nothing to write. There is deliberately
    /// no resolve-then-discard path — a result computed and then thrown away
    /// invites someone to later "fix" it by saving.
    Production,
}

#[derive(Debug, Clone)]
pub struct Installed {
    pub name: String,
    pub version: String,
}

/// One dependency to add on top of what the manifests already declare.
///
/// Adding a package is otherwise an ordinary sync: the request changes what
/// the target importer asks for, and the rest of the workspace is resolved,
/// linked and recorded exactly as it would be with nothing requested at all.
#[derive(Debug)]
pub struct Request {
    pub importer: ImporterPath,
    pub name: String,
    /// What the registry is asked: a range, an exact version, or a dist-tag.
    ///
    /// Deliberately still a `VersionSpec` rather than the string
    /// `as_request` flattens it to. Two of the three questions asked of it —
    /// does it rule nothing out, is it a range worth recording — do give the
    /// same answer for `Latest` as for a literal `latest`. The third does not:
    /// a manifest may declare `"lodash": "latest"`, and against one that does,
    /// a flattened seed would match the recorded specifier and reuse the
    /// lockfile. A bare `jerky install <pkg>` must always ask, because only
    /// the registry can say what a tag means today.
    pub seed: VersionSpec,
    /// Which section to record it under, when the command named one.
    ///
    /// `None` is `jerky install <pkg>`, which names a package and not a
    /// section. What it means is *whichever section this is already declared
    /// in*, and the manifest is where that answer lives, so it is read there
    /// rather than guessed here. `Some` is `--save-dev`: a statement about the
    /// section that outranks what the manifest currently says, and the only
    /// thing that moves a dependency between the two.
    pub kind: Option<Kind>,
}

/// What a manifest should record for a requested package.
#[derive(Debug)]
pub struct Recorded {
    pub name: String,
    /// The specifier to write: the range the user typed, or a pin.
    pub specifier: String,
    /// The concrete version that specifier resolved to.
    pub version: String,
    /// The section to write it to. Taken from the resolved graph rather than
    /// from the request, because a request that named no section has its
    /// answer only once the manifest's own has been folded in.
    pub kind: Kind,
}

/// What a sync did.
#[derive(Debug)]
pub struct Outcome {
    /// Every *registry* package the workspace now has linked, across every
    /// importer, deduplicated by `name@version` because the virtual store
    /// holds one entry per version however many importers asked for it.
    ///
    /// Workspace members are deliberately absent: they are linked in place
    /// rather than installed, so counting them would report work that did not
    /// happen and would change with nothing but a `workspaces` edit.
    pub linked: Vec<Installed>,
    /// Entries convergence found in an importer's `node_modules` and could not
    /// prove jerky had written, so left where they were.
    ///
    /// Carried rather than raised. A repository half-migrated from npm should
    /// be told what jerky declined to touch, not stopped — and the caller is
    /// the layer that knows how to say so, which is the same division
    /// `Workspace::warnings()` already draws.
    pub left_alone: Vec<Unowned>,
    /// Present only when there was a request to record.
    pub recorded: Option<Recorded>,
}

/// Make the workspace match what its manifests declare, plus `request`.
///
/// One walk seeds from every importer's ranges, so two projects wanting the
/// same package share the request and the store entry, and linking then runs
/// per importer because each owns its own `node_modules`.
///
/// A workspace of one is not a special case. It is a single importer keyed
/// `.`, and takes this path like any other.
///
/// Writing the lockfile is the last thing this does, and writing the manifest
/// is deliberately *not* its job — that belongs to the caller, above this, so
/// a failure anywhere leaves at worst an installed-but-unrecorded package
/// rather than a `package.json` claiming a dependency that is not on disk.
pub fn sync(
    workspace: &Workspace,
    store: &Store,
    registry: &dyn RegistryClient,
    request: Option<&Request>,
    mode: Mode,
) -> Result<Outcome, InstallError> {
    debug_assert!(
        request.is_none() || mode == Mode::Develop,
        "a production sync reproduces a lockfile and so has nothing to add to it; \
         the CLI refuses the combination at parse time"
    );

    if let Some(request) = request {
        // Before anything is looked up or written. A request that rules
        // nothing out is refused rather than pinned, because either reading of
        // it is a guess: recording `*` would put the widest possible drift
        // permission in a manifest whose whole default exists to avoid one,
        // and quietly pinning it instead would answer a question the user did
        // not ask.
        if let Some(unconstrained) = unconstrained_range(request.seed.as_request()) {
            return Err(InstallError::UnconstrainedRange {
                name: request.name.clone(),
                requested: unconstrained.to_string(),
            });
        }

        if !workspace.members().contains_key(&request.importer) {
            return Err(InstallError::UnknownImporter {
                importer: request.importer.to_string(),
                members: workspace
                    .members()
                    .keys()
                    .map(ImporterPath::to_string)
                    .collect(),
            });
        }
    }

    // Every importer's declared ranges seed the walk, not just the one being
    // installed into. That is what lets a single resolution answer for the
    // whole workspace, and what stops installing into `apps/web` from leaving
    // `packages/ui` unlinked.
    let declared: BTreeMap<ImporterPath, BTreeMap<String, Declared>> = workspace
        .members()
        .iter()
        .map(|(path, member)| (path.clone(), declared_by(&member.manifest)))
        .collect();

    // Name -> where it lives, for `workspace:` specifiers. A member with no
    // `name` cannot be depended on by name, so it is simply absent rather than
    // an error.
    let members: BTreeMap<String, ImporterPath> = workspace
        .members()
        .values()
        .filter_map(|member| {
            member
                .manifest
                .name()
                .map(|name| (name.to_string(), member.importer.clone()))
        })
        .collect();

    // Staleness is per importer, which is what the importers map buys over a
    // single block of ranges: editing `apps/web` must not invalidate what the
    // root already resolved.
    let locked = lockfile::load(workspace.root())?;

    // Before the store is touched and before a single link is written, so a
    // refusal leaves the tree exactly as it was found. Everything below this
    // point assumes the lockfile answers for every importer, which is what
    // makes the production path resolve nothing and write nothing.
    if mode == Mode::Production {
        let Some(locked) = locked.as_ref() else {
            return Err(InstallError::ProductionLockfileMissing {
                path: workspace.root().join(lockfile::LOCKFILE_NAME),
            });
        };
        if let Some(stale) = first_disagreement(locked, &declared) {
            return Err(stale);
        }
    }

    let reused = reusable_importers(locked.as_ref(), &declared, request);

    // Kept past the point where `locked` is consumed into the graph. When an
    // importer is reused these are the same bytes; it is a re-resolution that
    // can disagree with them, and that disagreement is the thing to catch.
    let locked_integrity: BTreeMap<PackageId, Integrity> = locked
        .iter()
        .flat_map(|lock| lock.packages.values())
        .map(|package| (package.id.clone(), package.integrity.clone()))
        .collect();

    let mut stale: BTreeMap<ImporterPath, BTreeMap<String, Declared>> = declared
        .iter()
        .filter(|(path, _)| !reused.contains_key(*path))
        .map(|(path, deps)| (path.clone(), deps.clone()))
        .collect();
    // Folded in only where the target is being re-resolved. If it was reused,
    // the request is already satisfied by what the lockfile recorded, and
    // adding it back would be asking a question that has an answer.
    //
    // A request that names a section gets it, which is the whole of what
    // `--save-dev` does below the CLI. One that names none keeps whatever the
    // manifest already declares, and is `Prod` only for a name the manifest
    // does not declare at all: `jerky install lodash@4.18.0` against a lodash
    // in `devDependencies` is a version change, and hardcoding `Prod` here
    // would not mean "no section was asked for" but would silently rewrite the
    // section the user chose. Nothing downstream would catch that — the
    // resolver is kind-blind by design, and `already_satisfies` compares only
    // the section the command itself asked for.
    if let Some(request) = request
        && let Some(deps) = stale.get_mut(&request.importer)
    {
        let kind = request.kind.unwrap_or_else(|| {
            deps.get(&request.name)
                .map_or(Kind::Prod, |declared| declared.kind)
        });
        deps.insert(
            request.name.clone(),
            Declared {
                specifier: request.seed.as_request().to_string(),
                kind,
            },
        );
    }

    let mut graph = match (stale.is_empty(), locked) {
        // Nothing to ask: every importer matched, so the lockfile *is* the
        // answer and the registry is never touched.
        (true, Some(lock)) => ResolvedGraph {
            importers: reused,
            packages: lock.packages,
        }
        .reachable(),
        (_, lock) => {
            let resolved = resolver::resolve(registry, &stale, &members)?;
            let locked_packages = lock.map(|lock| lock.packages).unwrap_or_default();
            ResolvedGraph {
                importers: reused.into_iter().chain(resolved.importers).collect(),
                packages: locked_packages
                    .into_iter()
                    .chain(resolved.packages)
                    .collect(),
            }
            .reachable()
        }
    };

    // `dependencies` only, which is the whole of what a production install
    // differs by. Dropping the edges and recomputing reachability is enough:
    // `reachable` keeps a package while some importer still leads to it, so a
    // package only a devDependency wanted falls out on its own and nothing
    // below here has to reason about kinds a second time. Convergence and the
    // virtual store prune then follow the graph as they always do, which is
    // why `--production` *removes* devDependency links rather than merely
    // declining to write them.
    if mode == Mode::Production {
        for importer in graph.importers.values_mut() {
            importer
                .dependencies
                .retain(|_, dep| dep.kind == Kind::Prod);
        }
        graph = graph.reachable();
    }

    // The locked-integrity gate, over every package, before a single byte is
    // fetched. It was previously folded into the materialisation loop, which
    // meant a mismatch on a late package was raised only after every earlier
    // one had been downloaded. Fan-out makes that worse rather than merely
    // untidy: with sixteen workers in flight there is no "earlier", so the
    // gate has to be a pass of its own to keep meaning what it says.
    //
    // Before the store is consulted, not only on the download path.
    //
    // #47 proposed the latter, reasoning that a store hit already proves the
    // bytes hash to the recorded key. That is true and beside the point: it
    // compares bytes against their *own* hash, while this compares the
    // *locked* hash against the *reported* one. The key here is derived from
    // what the registry now says, so a republished tarball whose bytes some
    // other project already put in the machine-global store takes the hit path
    // and is never questioned — which is the precise attack the lockfile
    // exists to catch.
    //
    // The cost #47 wanted to avoid was re-fetching metadata for an entry
    // already present. Nothing is re-fetched: both hashes are already in
    // memory. A package carried over from the lockfile rather than re-resolved
    // compares equal by construction.
    //
    // The comparison is algorithm-sensitive, though, and that is the one way
    // it fires without tampering: `Integrity` compares algo *and* digest, so
    // an entry locked from a legacy `dist.shasum` (sha1) whose metadata later
    // carries a `dist.integrity` (sha512) disagrees with itself. The bytes are
    // fine and the error says otherwise, with no path out but editing the
    // lockfile by hand. Narrowing that needs a decision about which side to
    // re-hash and is deliberately not made here — but it is the reason this is
    // not the "cannot fire spuriously" check an earlier draft of this comment
    // claimed.
    for (id, package) in &graph.packages {
        if let Some(locked) = locked_integrity.get(id)
            && *locked != package.integrity
        {
            return Err(InstallError::LockedIntegrityMismatch {
                name: id.name.clone(),
                version: id.version.clone(),
                locked: locked.to_ssri(),
                reported: package.integrity.to_ssri(),
            });
        }
    }

    // Every tarball the store lacks, fetched concurrently. Packages the store
    // already holds cost nothing here and are never downloaded.
    fetch_missing(&graph, store, registry)?;

    // Say what the tree should be, then make it so. Two statements, because
    // the ordering the second half obeys — entries before the links into them,
    // convergence after linking, the prune last — is a property of `apply`
    // rather than of the order these lines happen to be written in. It used to
    // be five loops here and a comment above each one saying what must not be
    // moved.
    let left_alone = plan_for(&graph, workspace, store).apply()?;

    // The lockfile records what the manifest declares, so the specifier it
    // carries for this request is the one about to be written rather than the
    // seed resolution was given. Were they allowed to differ, every install
    // would find its own lockfile stale and re-resolve a workspace nothing had
    // touched.
    let recorded = request.map(|request| {
        let recorded = record_for(request, &graph, workspace);
        graph
            .importers
            .get_mut(&request.importer)
            .and_then(|resolved| resolved.dependencies.get_mut(&request.name))
            .expect("the request was either resolved into the target or reused from it")
            .specifier = recorded.specifier.clone();
        recorded
    });

    // Not on the production path, where the lockfile is the input rather than
    // the output. There is nothing to write even in principle — every importer
    // matched, so re-serializing would reproduce the same bytes — and saying so
    // with a branch rather than relying on that is what keeps the guarantee
    // from depending on serialization being perfectly stable.
    if mode == Mode::Develop {
        lockfile::save(&graph, workspace.root())?;
    }

    Ok(Outcome {
        linked: graph
            .packages
            .keys()
            .map(|id| Installed {
                name: id.name.clone(),
                version: id.version.clone(),
            })
            .collect(),
        left_alone,
        recorded,
    })
}

/// The first place the lockfile and the manifests disagree, as the error to
/// raise for it.
///
/// Deliberately reports *one* dependency rather than a count or a list. The
/// failure this guards is a manifest edited without reinstalling, so what the
/// user needs is the edit — naming it, and both values, turns "your lockfile is
/// stale" into something they can act on without a diff.
///
/// The walk is over both kinds, not only the ones a production install would
/// link. A stale `devDependencies` block means the lockfile is genuinely out of
/// date, and a mode that overlooked it would let CI pass on exactly the file it
/// exists to verify.
///
/// An importer absent from the lockfile is read as one that records nothing,
/// so a member added to `workspaces` without reinstalling reports its first
/// dependency as unrecorded rather than needing a shape of its own. Both maps
/// are `BTreeMap`s, so which disagreement comes first is a property of the
/// names rather than of iteration luck.
fn first_disagreement(
    locked: &ResolvedGraph,
    declared: &BTreeMap<ImporterPath, BTreeMap<String, Declared>>,
) -> Option<InstallError> {
    let empty = Importer::default();

    for (path, manifest_declares) in declared {
        let recorded = locked.importers.get(path).unwrap_or(&empty);

        for (name, declared) in manifest_declares {
            let disagrees = match recorded.dependencies.get(name) {
                None => true,
                Some(dependency) => {
                    dependency.specifier != declared.specifier || dependency.kind != declared.kind
                }
            };
            if disagrees {
                return Some(InstallError::ProductionLockfileStale {
                    importer: path.to_string(),
                    name: name.clone(),
                    declared: describe_declared(&declared.specifier, declared.kind),
                    locked: recorded.dependencies.get(name).map_or_else(
                        || "nothing".to_string(),
                        |dependency| describe_declared(&dependency.specifier, dependency.kind),
                    ),
                });
            }
        }

        // The other direction: a dependency deleted from the manifest lingers
        // in the lockfile, which is the same staleness seen from the far side.
        if let Some((name, dependency)) = recorded
            .dependencies
            .iter()
            .find(|(name, _)| !manifest_declares.contains_key(*name))
        {
            return Some(InstallError::ProductionLockfileStale {
                importer: path.to_string(),
                name: name.clone(),
                declared: "nothing".to_string(),
                locked: describe_declared(&dependency.specifier, dependency.kind),
            });
        }
    }

    // And the same question one level up: a whole importer the lockfile
    // records that the workspace no longer has, from a member dropped out of
    // `workspaces` or a directory deleted. Nothing above can see it, because
    // the walk is driven by what the manifests declare and this importer is
    // exactly the one none of them do. Left unchecked it is the mode's own
    // failure in miniature — `--production` would report success on a lockfile
    // that `jerky install` rewrites, which is the CI-passes-on-a-stale-file
    // case the whole mode exists to refuse.
    //
    // Reported after the per-dependency walk, so the more specific message
    // wins when a repository manages both at once.
    if let Some(path) = locked
        .importers
        .keys()
        .find(|path| !declared.contains_key(*path))
    {
        return Some(InstallError::ProductionLockfileImporterGone {
            importer: path.to_string(),
        });
    }

    None
}

/// `` `^4.0.0` in dependencies `` — a specifier is not enough on its own.
///
/// A dependency moved between sections at an unchanged specifier is a real
/// edit and a real staleness, and an error that printed only the specifier
/// would read as `^4.0.0` disagreeing with `^4.0.0`.
///
/// The section name comes from `manifest`, which already owns that mapping,
/// rather than being spelled again here: `docs/agents/invariants.md` counts
/// the modules where these names appear, and a label is not a good enough
/// reason to become a third.
fn describe_declared(specifier: &str, kind: Kind) -> String {
    format!("`{specifier}` in {}", crate::manifest::section_for(kind))
}

/// Everything one member declares, both sections in one map.
///
/// Flat and keyed by name, matching the graph: a name can only mean one thing,
/// so a manifest declaring the same package in both sections has to be
/// reconciled here rather than carried forward as a contradiction.
///
/// `dependencies` wins, which is npm's answer and jerky's. The alternative —
/// refusing the manifest — assumes the user can edit it, and the manifest in
/// front of jerky may belong to a member of someone else's monorepo or be
/// generated by a tool. Worth saying out loud, because silently dropping one
/// of two declarations otherwise reads as a bug.
fn declared_by(manifest: &Manifest) -> BTreeMap<String, Declared> {
    let dev = manifest.dev_dependencies().into_iter().map(|(name, spec)| {
        (
            name,
            Declared {
                specifier: spec,
                kind: Kind::Dev,
            },
        )
    });
    let prod = manifest.dependencies().into_iter().map(|(name, spec)| {
        (
            name,
            Declared {
                specifier: spec,
                kind: Kind::Prod,
            },
        )
    });

    // `prod` second, so it overwrites a name `dev` already inserted.
    dev.chain(prod).collect()
}

/// Download, verify and commit every package the store does not already hold.
///
/// Split out of the materialisation loop because it is the only part of an
/// install that waits on the network, and therefore the only part worth
/// overlapping. Once the graph is resolved every tarball URL is known, so this
/// is a fixed work list rather than a walk that discovers as it goes — which is
/// what makes a plain bounded pool sufficient and keeps the resolver's harder
/// problem (#33's second half) out of scope here.
///
/// Verification stays per-tarball and stays ahead of extraction: a worker holds
/// the complete buffer, checks it against the hash the graph carries, and only
/// then hands it to `store.commit`. Parallelism is not a reason to stream.
fn fetch_missing(
    graph: &ResolvedGraph,
    store: &Store,
    registry: &dyn RegistryClient,
) -> Result<(), InstallError> {
    // Decided up front, on one thread. Asking the store mid-flight would race
    // the commits this function is itself performing, and a package listed
    // twice would be downloaded twice.
    let missing: Vec<(&PackageId, &ResolvedPackage)> = graph
        .packages
        .iter()
        .filter(|(_, package)| !store.contains(&package.integrity))
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    // `missing` is in `graph.packages` order, so the failure `drain` reports —
    // the lowest-indexed one — is chosen by where the package sits in the
    // graph rather than by which worker happened to lose. An install that
    // blamed a different package on each run would be untriageable.
    //
    // A fetch already in flight when another fails still finishes and may
    // commit, which is correct here: the store is a machine-global cache of
    // verified bytes, and an entry landing in it is not a claim that any
    // project depends on it. What must not happen is a link or a manifest
    // edit, and this function reaches neither.
    pool::drain(&missing, MAX_CONCURRENT_FETCHES, |(id, package)| {
        fetch_one(id, package, store, registry)
    })
}

/// One package: fetch, verify, extract into the store.
fn fetch_one(
    id: &PackageId,
    package: &ResolvedPackage,
    store: &Store,
    registry: &dyn RegistryClient,
) -> Result<(), InstallError> {
    let tarball = registry.fetch_tarball(&package.resolved)?;

    // Verify the complete buffer before a single byte is extracted. A
    // stream-and-hash design would only detect a mismatch after writing
    // attacker-controlled files to disk.
    package
        .integrity
        .verify(&tarball)
        .map_err(|source| InstallError::Integrity {
            name: id.name.clone(),
            version: id.version.clone(),
            source,
        })?;

    // `commit` stages and renames, and treats losing a race as success because
    // the key is the content hash. Two workers asked for the same entry —
    // which cannot happen within one install, but can across concurrent
    // processes — both end up correct.
    store.commit(&package.integrity, |staging| {
        archive::extract(&tarball, staging)
    })?;

    Ok(())
}

/// The tree this graph means, as a plan the linker can apply.
///
/// Three descriptions and nothing performed: the virtual store the workspace
/// root should hold, the edges that leave each entry in it, and one
/// `node_modules` per importer. What order those get written in, and what gets
/// removed afterwards, is [`linker::Plan::apply`]'s business — this says only
/// what the answer is.
///
/// Every importer in the graph is named, including one that declares nothing.
/// That is not a formality: convergence is what unlinks a dropped dependency,
/// and the importer that just lost its last one has no links to write and
/// everything to remove.
fn plan_for(graph: &ResolvedGraph, workspace: &Workspace, store: &Store) -> linker::Plan {
    // Every member's directory, so that a link at one is recognised as jerky's.
    // A local link points straight at the member rather than into the virtual
    // store, and an ownership test that knew only about the store would delete
    // every local link on the install after it was created.
    let members: BTreeSet<PathBuf> = workspace
        .members()
        .values()
        .map(|member| member.path.clone())
        .collect();

    // The virtual store sits at the workspace root, which is the whole reason
    // two importers on one version share an entry rather than each unpacking
    // their own. The plan carries the root and derives the rest.
    let mut plan = linker::Plan::new(workspace.root(), members);

    for (id, package) in &graph.packages {
        plan.add_entry(
            &id.to_string(),
            linker::VirtualStoreEntry {
                store_path: store.entry_path(&package.integrity),
                pkg_name: id.name.clone(),
                edges: package
                    .dependencies
                    .iter()
                    .map(|(name, dep_id)| (name.clone(), store_entry(dep_id)))
                    .collect(),
            },
        );
    }

    for (path, resolved) in &graph.importers {
        let member = workspace
            .members()
            .get(path)
            .expect("every importer in the graph was seeded from a member");

        let links = resolved
            .dependencies
            .iter()
            .map(|(name, dependency)| {
                let target = match &dependency.resolution {
                    Resolution::Registry(id) => linker::ImporterTarget::Entry(store_entry(id)),
                    // The graph records a path relative to the declaring
                    // importer, which is what the lockfile wants. Linking uses
                    // the member's own absolute directory instead: joining a
                    // relative target back on would produce a path full of
                    // `..` components, and the linker compares paths lexically.
                    Resolution::Local(_) => {
                        let local = workspace
                            .member_by_name(name)
                            .expect("a local resolution named a member the resolver found");
                        linker::ImporterTarget::Member(local.path.clone())
                    }
                };
                (name.clone(), target)
            })
            .collect();

        plan.add_importer(&member.path, links);
    }

    plan
}

/// Where a resolved package sits in the virtual store.
///
/// The one place that pairs the two, so a caller cannot get the order wrong:
/// the entry's directory is the package's `name@version`, and the package
/// nested inside it is named for the package itself — which for an alias is
/// not the name the link takes.
fn store_entry(id: &PackageId) -> linker::StoreEntry {
    linker::StoreEntry {
        dir_name: id.to_string(),
        pkg_name: id.name.clone(),
    }
}

/// What the manifest should record for a request, and what it resolved to.
///
/// The two differ more often than they look like they should, which is why
/// this is one named function rather than an expression at the call site.
fn record_for(request: &Request, graph: &ResolvedGraph, workspace: &Workspace) -> Recorded {
    let resolved = &graph.importers[&request.importer].dependencies[&request.name];

    let (specifier, version) = match &resolved.resolution {
        // The exact version that was chosen, never a range invented over it.
        // jerky pins by *default*: a caret would hand the next install
        // permission to pick a version nobody asked for, and the difference
        // only surfaces later, on whichever machine re-resolved first.
        //
        // A range the user typed is not the default, though, and overwriting
        // it with a pin would be the tool overruling an instruction rather
        // than supplying a missing one. `jerky install lodash@^4.0.0` records
        // `^4.0.0`; a dist-tag still pins, because `latest` in a manifest is a
        // moving pointer rather than a constraint.
        // An alias keeps its scheme, with the pin or the range *inside* it.
        // Recording the bare version would leave a manifest naming a
        // `width-cjs@4.2.3` no registry serves, which is #73's "must not
        // half-work": the link would be right and the next install wrong.
        Resolution::Registry(id) => {
            let seed = request.seed.as_request();
            let specifier = match resolver::alias_target(seed) {
                Some((aliased, range)) => format!(
                    "{ALIAS_PROTOCOL}{aliased}@{}",
                    declared_range(range).unwrap_or(&id.version)
                ),
                None => declared_range(seed).unwrap_or(&id.version).to_string(),
            };
            (specifier, id.version.clone())
        }
        // `jerky install ui@workspace:*` names a member on purpose. The link
        // is already written by the loop above; what differs is what gets
        // recorded — the protocol as asked for, never a version, because the
        // member's version is whatever the repo says today and pinning it
        // would go stale on the next commit to that member.
        Resolution::Local(_) => {
            let member = workspace
                .member_by_name(&request.name)
                .expect("a local resolution named a member the resolver found");
            (
                resolved.specifier.clone(),
                member.manifest.version().unwrap_or("local").to_string(),
            )
        }
    };

    Recorded {
        name: request.name.clone(),
        specifier,
        version,
        // Whatever the graph carries: the request's own section where it named
        // one, the manifest's where it did not, and — on the reused path,
        // where neither was folded in — the lockfile's, which is the
        // manifest's by the definition of having been reusable at all.
        kind: resolved.kind,
    }
}

/// Install one package into `importer`, resolving the whole workspace.
///
/// A thin shell over `sync`: turn the command's spec into a request, let the
/// sync do the work, then record the result.
///
/// `kind` is the section the command asked for — `Some(Kind::Dev)` for
/// `--save-dev`, and `None` where it asked for none, which leaves the section
/// to whatever the manifest already says. Nothing here works that out: the
/// answer comes back in `Recorded`, decided once inside the sync, where what
/// each manifest declares has already been gathered.
///
/// Ordering is deliberate and unchanged. The manifest write is last, so a
/// failure anywhere leaves at worst an installed-but-unrecorded package —
/// harmless and self-healing on rerun — rather than a `package.json` claiming
/// a dependency that is not on disk.
///
/// The sync's whole `Outcome` comes back rather than only the package that was
/// added, because what was added is not the only thing above this that has to
/// be said out loud: convergence reports the entries it declined to touch, and
/// swallowing them here would leave `jerky install <pkg>` silent about exactly
/// the half-migrated repository that report exists for. `recorded` is always
/// `Some` on this path — a sync given a request always reports what to record.
pub fn install(
    workspace: &Workspace,
    importer: &ImporterPath,
    store: &Store,
    registry: &dyn RegistryClient,
    spec: &PackageSpec,
    kind: Option<Kind>,
) -> Result<Outcome, InstallError> {
    let request = Request {
        importer: importer.clone(),
        name: spec.name.clone(),
        seed: spec.version.clone(),
        kind,
    };

    let outcome = sync(workspace, store, registry, Some(&request), Mode::Develop)?;
    let recorded = outcome
        .recorded
        .as_ref()
        .expect("a sync given a request always reports what to record");

    // `sync` validated membership before touching anything, so the target is
    // known to exist by the time this runs.
    let target = &workspace.members()[importer];

    // Last: never record something that is not already true on disk.
    //
    // Which of the two writes is the whole difference `--save-dev` makes to
    // the file. A command that named a section is moving the dependency into
    // it, so the declaration it is leaving has to go. A command that named
    // none is changing a version, and a manifest that happens to declare the
    // name in both sections is a contradiction it was not asked to settle —
    // deleting the other entry there would lose a line the user never
    // mentioned, on an install that only asked for a different version.
    let mut manifest = Manifest::load(&target.path)?;
    if kind.is_some() {
        manifest.move_dependency(&recorded.name, &recorded.specifier, recorded.kind);
    } else {
        manifest.add_dependency(&recorded.name, &recorded.specifier, recorded.kind);
    }
    manifest.save()?;

    Ok(outcome)
}

/// The importers whose recorded specifiers still match their manifests.
///
/// An importer that matches is taken from the lockfile verbatim; one that does
/// not is re-resolved. The comparison is on specifiers rather than on resolved
/// versions because a specifier is what the user wrote, and it is the only
/// thing that can go stale without jerky having done it.
fn reusable_importers(
    locked: Option<&ResolvedGraph>,
    declared: &BTreeMap<ImporterPath, BTreeMap<String, Declared>>,
    request: Option<&Request>,
) -> BTreeMap<ImporterPath, Importer> {
    let Some(locked) = locked else {
        return BTreeMap::new();
    };

    declared
        .iter()
        .filter_map(|(path, manifest_declares)| {
            let recorded = locked.importers.get(path)?;
            let targeted = request.filter(|request| &request.importer == path);
            let fresh = recorded.matches(manifest_declares)
                && targeted.is_none_or(|request| already_satisfies(recorded, request));
            fresh.then(|| (path.clone(), recorded.clone()))
        })
        .collect()
}

/// Is the command's own request already answered by what was recorded?
///
/// The manifest can match the lockfile perfectly and still not answer the
/// question being asked — `jerky install lodash@4.18.0` against a lockfile
/// holding 4.17.21 is a new request, not a no-op.
///
/// Two axes, and the same two the lockfile records: the version asked for and
/// the section asked for. Either one differing from what was recorded is a
/// real request.
///
/// This is deliberately *not* expressible as folding the request into what the
/// importer declares and then asking `matches`. A request naming the version
/// the lockfile already resolved to — `lodash@4.18.0` where the manifest says
/// `^4.0.0` — is answered, yet differs from the declared specifier, so the
/// fold would re-resolve a question that already has its answer.
fn already_satisfies(recorded: &Importer, request: &Request) -> bool {
    let Some(dependency) = recorded.dependencies.get(&request.name) else {
        return false;
    };

    // A section is as much of a request as a version is. `--save-dev` on a
    // package recorded under `dependencies` is answered by no version the
    // lockfile holds, and reusing the importer would move the manifest entry
    // while leaving the lockfile describing the section the manifest just
    // left — the same staleness `Importer::matches` refuses to overlook when
    // that edit arrives by hand instead. Re-resolving the importer is what
    // saying so costs; the alternative is a lockfile that disagrees with the
    // manifest written by the same command.
    if request.kind.is_some_and(|kind| kind != dependency.kind) {
        return false;
    }

    match &request.seed {
        // Only the registry can say what `latest` means today, so a bare
        // `jerky install <pkg>` always asks. That is the request, not a
        // shortcoming of the lockfile — and it is why the seed is not
        // flattened to a string, since a manifest declaring `"lodash":
        // "latest"` would otherwise match here.
        VersionSpec::Latest => false,
        // Either the request is the specifier verbatim — which is how
        // `workspace:*` and a typed range both match — or it names the
        // version that was chosen.
        VersionSpec::Exact(requested) => {
            dependency.specifier == *requested
                || matches!(&dependency.resolution, Resolution::Registry(id) if id.version == *requested)
        }
    }
}

/// The request, when it is a range that rules nothing out.
///
/// What counts as ruling nothing out is `Range`'s question, not this module's
/// — `*` has several spellings and the test is on what a range admits.
fn unconstrained_range(seed: &str) -> Option<&str> {
    Range::parse(seed).ok()?.admits_everything().then_some(seed)
}

/// The range the user typed, when what they typed was a range.
///
/// A bare version is excluded on purpose: `4.17.21` is a valid range matching
/// exactly itself, but recording it as one would make every pin
/// indistinguishable from a deliberate constraint. A dist-tag is excluded by
/// the same test without needing its own arm — `latest` does not parse as a
/// range at all — and must not be recorded verbatim, because in a manifest it
/// names whatever the registry means by it on some later day rather than the
/// thing that was installed.
fn declared_range(seed: &str) -> Option<&str> {
    (Range::parse(seed).is_ok() && Version::parse(seed).is_err()).then_some(seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::integrity::Algo;
    use crate::linker::{ImporterTarget, Plan, StoreEntry, VirtualStoreEntry};
    use crate::resolver::Dependency;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, json: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("package.json"), json).unwrap();
    }

    fn id(name: &str, version: &str) -> PackageId {
        PackageId {
            name: name.to_string(),
            version: version.to_string(),
        }
    }

    /// Distinct per package, because the store path a plan carries is derived
    /// from the integrity and two entries must not share one.
    fn integrity(seed: &str) -> Integrity {
        Integrity {
            algo: Algo::Sha512,
            digest: <sha2::Sha512 as sha2::Digest>::digest(seed.as_bytes()).to_vec(),
        }
    }

    fn package(name: &str, version: &str, dependencies: &[(&str, PackageId)]) -> ResolvedPackage {
        ResolvedPackage {
            id: id(name, version),
            resolved: format!("https://fixture.test/{name}-{version}.tgz"),
            integrity: integrity(&format!("{name}@{version}")),
            dependencies: dependencies
                .iter()
                .map(|(link, target)| (link.to_string(), target.clone()))
                .collect(),
        }
    }

    fn dependency(specifier: &str, resolution: Resolution) -> Dependency {
        Dependency {
            specifier: specifier.to_string(),
            kind: Kind::Prod,
            resolution,
        }
    }

    fn importer(dependencies: &[(&str, Dependency)]) -> Importer {
        Importer {
            dependencies: dependencies
                .iter()
                .map(|(name, dep)| (name.to_string(), dep.clone()))
                .collect(),
        }
    }

    fn at(path: &str) -> ImporterPath {
        ImporterPath::new(path).unwrap()
    }

    fn store_dir(dir_name: &str, pkg_name: &str) -> StoreEntry {
        StoreEntry {
            dir_name: dir_name.to_string(),
            pkg_name: pkg_name.to_string(),
        }
    }

    /// A workspace of three: the root, a member with dependencies, and a
    /// member that declares nothing.
    fn workspace(root: &Path) -> Workspace {
        write_manifest(root, r#"{"name":"ws","workspaces":["packages/*"]}"#);
        write_manifest(&root.join("packages/ui"), r#"{"name":"ui"}"#);
        write_manifest(&root.join("packages/empty"), r#"{"name":"empty"}"#);
        Workspace::discover(root).unwrap()
    }

    #[test]
    fn the_plan_names_exactly_the_entries_edges_and_links_the_graph_implies() {
        // The seam the orchestrator now speaks across. Written as an equality
        // against a plan built by hand through the same public interface,
        // because what is under test is that nothing is named twice, nothing
        // is missed, and no name is swapped for another — an assertion made
        // one `contains` at a time can pass while missing all three.
        let home = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let workspace = workspace(work.path());
        let store = Store::new(home.path().join("store"));
        // `discover` canonicalizes, so every path below is taken from the
        // workspace rather than from the temp directory it was built in.
        let root = workspace.root().to_path_buf();

        let graph = ResolvedGraph {
            importers: BTreeMap::from([
                (
                    at("."),
                    importer(&[(
                        "alpha",
                        dependency("1.0.0", Resolution::Registry(id("alpha", "1.0.0"))),
                    )]),
                ),
                (
                    at("packages/ui"),
                    importer(&[
                        (
                            // An alias: the link takes the local name, the
                            // entry is the real package's.
                            "width-cjs",
                            dependency(
                                "npm:string-width@^4.0.0",
                                Resolution::Registry(id("string-width", "4.2.3")),
                            ),
                        ),
                        (
                            "empty",
                            dependency("workspace:*", Resolution::Local("../empty".into())),
                        ),
                    ]),
                ),
                // Declares nothing, and is named all the same: convergence is
                // what unlinks a dropped dependency, and the importer that
                // just lost its last one has nothing to write and everything
                // to remove.
                (at("packages/empty"), importer(&[])),
            ]),
            packages: BTreeMap::from([
                (
                    id("alpha", "1.0.0"),
                    package(
                        "alpha",
                        "1.0.0",
                        &[
                            ("beta", id("beta", "2.0.0")),
                            ("width-cjs", id("string-width", "4.2.3")),
                        ],
                    ),
                ),
                (id("beta", "2.0.0"), package("beta", "2.0.0", &[])),
                (
                    id("string-width", "4.2.3"),
                    package("string-width", "4.2.3", &[]),
                ),
            ]),
        };

        let mut expected = Plan::new(
            &root,
            BTreeSet::from([
                root.clone(),
                root.join("packages/ui"),
                root.join("packages/empty"),
            ]),
        );
        expected.add_entry(
            "alpha@1.0.0",
            VirtualStoreEntry {
                store_path: store.entry_path(&integrity("alpha@1.0.0")),
                pkg_name: "alpha".to_string(),
                edges: BTreeMap::from([
                    ("beta".to_string(), store_dir("beta@2.0.0", "beta")),
                    (
                        "width-cjs".to_string(),
                        store_dir("string-width@4.2.3", "string-width"),
                    ),
                ]),
            },
        );
        expected.add_entry(
            "beta@2.0.0",
            VirtualStoreEntry {
                store_path: store.entry_path(&integrity("beta@2.0.0")),
                pkg_name: "beta".to_string(),
                edges: BTreeMap::new(),
            },
        );
        expected.add_entry(
            "string-width@4.2.3",
            VirtualStoreEntry {
                store_path: store.entry_path(&integrity("string-width@4.2.3")),
                pkg_name: "string-width".to_string(),
                edges: BTreeMap::new(),
            },
        );
        expected.add_importer(
            &root,
            BTreeMap::from([(
                "alpha".to_string(),
                ImporterTarget::Entry(store_dir("alpha@1.0.0", "alpha")),
            )]),
        );
        expected.add_importer(
            &root.join("packages/ui"),
            BTreeMap::from([
                (
                    "width-cjs".to_string(),
                    ImporterTarget::Entry(store_dir("string-width@4.2.3", "string-width")),
                ),
                // Absolute, and the member's own directory rather than the
                // `../empty` the graph records: the lockfile wants a path
                // relative to the declaring importer, and the linker compares
                // paths lexically.
                (
                    "empty".to_string(),
                    ImporterTarget::Member(root.join("packages/empty")),
                ),
            ]),
        );
        expected.add_importer(&root.join("packages/empty"), BTreeMap::new());

        assert_eq!(plan_for(&graph, &workspace, &store), expected);
    }
}
