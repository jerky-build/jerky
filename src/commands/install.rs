use std::collections::BTreeMap;
use std::path::PathBuf;

use thiserror::Error;

use crate::archive::{self, ArchiveError};
use crate::cli::{PackageSpec, VersionSpec};
use crate::integrity::{Integrity, IntegrityError};
use crate::linker::{self, LinkError};
use crate::lockfile::{self, LockfileError};
use crate::manifest::{Manifest, ManifestError};
use crate::registry::{RegistryClient, RegistryError};
use crate::resolver::{
    self, Importer, ImporterPath, PackageId, Resolution, ResolveError, ResolvedGraph,
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
    #[error("`{importer}` is not a member of this workspace (members: {})", members.join(", "))]
    UnknownImporter {
        importer: String,
        members: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Installed {
    pub name: String,
    pub version: String,
}

/// Install one package into `importer`, resolving the whole workspace.
///
/// The phase structure from the single-project version holds — resolve, then
/// fill the store, then link, then record. What changes is the breadth: one
/// walk seeds from every importer's ranges, so two projects wanting the same
/// package share the request and the store entry, and linking then runs per
/// importer because each owns its own `node_modules`.
///
/// A workspace of one is not a special case. It is a single importer keyed
/// `.`, and takes this path like any other.
///
/// Ordering is deliberate and unchanged. The manifest write is last, so a
/// failure anywhere leaves at worst an installed-but-unrecorded package —
/// harmless and self-healing on rerun — rather than a `package.json` claiming
/// a dependency that is not on disk.
pub fn install(
    workspace: &Workspace,
    importer: &ImporterPath,
    store: &Store,
    registry: &dyn RegistryClient,
    spec: &PackageSpec,
) -> Result<Installed, InstallError> {
    let target =
        workspace
            .members()
            .get(importer)
            .ok_or_else(|| InstallError::UnknownImporter {
                importer: importer.to_string(),
                members: workspace
                    .members()
                    .keys()
                    .map(ImporterPath::to_string)
                    .collect(),
            })?;

    // Every importer's declared ranges seed the walk, not just the one being
    // installed into. That is what lets a single resolution answer for the
    // whole workspace, and what stops installing into `apps/web` from leaving
    // `packages/ui` unlinked.
    let declared: BTreeMap<ImporterPath, BTreeMap<String, String>> = workspace
        .members()
        .iter()
        .map(|(path, member)| (path.clone(), member.manifest.dependencies()))
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
    let reused = reusable_importers(locked.as_ref(), &declared, importer, spec);

    // Kept past the point where `locked` is consumed into the graph. When an
    // importer is reused these are the same bytes; it is a re-resolution that
    // can disagree with them, and that disagreement is the thing to catch.
    let locked_integrity: BTreeMap<PackageId, Integrity> = locked
        .iter()
        .flat_map(|lock| lock.packages.values())
        .map(|package| (package.id.clone(), package.integrity.clone()))
        .collect();

    let mut stale: BTreeMap<ImporterPath, BTreeMap<String, String>> = declared
        .iter()
        .filter(|(path, _)| !reused.contains_key(*path))
        .map(|(path, deps)| (path.clone(), deps.clone()))
        .collect();
    // Folded in only where the target is being re-resolved. If it was reused,
    // the request is already satisfied by what the lockfile recorded, and
    // adding it back would be asking a question that has an answer.
    if let Some(deps) = stale.get_mut(importer) {
        deps.insert(spec.name.clone(), spec.version.as_request().to_string());
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

    // The virtual store sits at the workspace root, which is the whole reason
    // two importers on one version share an entry rather than each unpacking
    // their own.
    let node_modules = workspace.root().join("node_modules");
    let mut entries: BTreeMap<PackageId, PathBuf> = BTreeMap::new();

    for (id, package) in &graph.packages {
        let key = package.integrity.store_key();

        // Populating is skipped entirely when the store already holds these
        // bytes, so a repeat install performs no download.
        // Before the store is consulted, not only on the download path.
        //
        // #47 proposed the latter, reasoning that a store hit already proves
        // the bytes hash to the recorded key. That is true and beside the
        // point: it compares bytes against their *own* hash, while this
        // compares the *locked* hash against the *reported* one. The key here
        // is derived from what the registry now says, so a republished tarball
        // whose bytes some other project already put in the machine-global
        // store takes the hit path and is never questioned — which is the
        // precise attack the lockfile exists to catch.
        //
        // The cost #47 wanted to avoid was re-fetching metadata for an entry
        // already present. Nothing is re-fetched: both hashes are already in
        // memory. A package carried over from the lockfile rather than
        // re-resolved compares equal by construction, so this cannot fire
        // spuriously either.
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

        let entry = if store.contains(&key) {
            store.entry_path(&key)
        } else {
            let tarball = registry.fetch_tarball(&package.resolved)?;

            // Verify the complete buffer before a single byte is extracted. A
            // stream-and-hash design would only detect a mismatch after
            // writing attacker-controlled files to disk.
            package
                .integrity
                .verify(&tarball)
                .map_err(|source| InstallError::Integrity {
                    name: id.name.clone(),
                    version: id.version.clone(),
                    source,
                })?;

            store.commit(&key, |staging| archive::extract(&tarball, staging))?
        };

        let virtual_dir =
            linker::populate_virtual_store(&entry, &node_modules, &id.to_string(), &id.name)?;
        entries.insert(id.clone(), virtual_dir);
    }

    // Each package's own edges, linked inside the store. This is what makes a
    // package see exactly what it declared: its siblings in its private
    // `node_modules` are its dependencies and nothing else.
    for (id, package) in &graph.packages {
        let owner = &entries[id];
        for (dep_name, dep_id) in &package.dependencies {
            linker::symlink_into_store(owner, dep_name, &dep_id.to_string())?;
        }
    }

    // Then one `node_modules` per importer, with targets shaped by how deep
    // that importer sits.
    for (path, resolved) in &graph.importers {
        let member = workspace
            .members()
            .get(path)
            .expect("every importer in the graph was seeded from a member");

        for (name, dependency) in &resolved.dependencies {
            match &dependency.resolution {
                Resolution::Registry(id) => linker::symlink_dependency_from(
                    &member.path,
                    workspace.root(),
                    name,
                    &id.to_string(),
                )?,
                // The graph records a path relative to the declaring importer,
                // which is what the lockfile wants. Linking uses the member's
                // own absolute directory instead: joining a relative target
                // back on would produce a path full of `..` components, and
                // the linker compares paths lexically.
                Resolution::Local(_) => {
                    let local = members
                        .get(name)
                        .and_then(|importer| workspace.members().get(importer))
                        .expect("a local resolution named a member the resolver found");
                    linker::symlink_local(&member.path, name, &local.path)?
                }
            }
        }
    }

    let requested = graph.importers[importer].dependencies[&spec.name].clone();
    let (recorded, reported) = match &requested.resolution {
        // The exact version that was chosen, never a range over it. jerky
        // pins by default: a caret would hand the next install permission to
        // pick a version nobody asked for, and the difference only shows up
        // later, on a machine that resolved at a different time. Widening it
        // is an edit the user can make and the resolver now honours; narrowing
        // a range back down after it has already drifted is not.
        Resolution::Registry(id) => (id.version.clone(), id.version.clone()),
        // `jerky install ui@workspace:*` names a member on purpose. The link
        // is already written by the loop above; what differs is what gets
        // recorded — the protocol as asked for, never a version, because the
        // member's version is whatever the repo says today and pinning it
        // would go stale on the next commit to that member.
        Resolution::Local(_) => {
            let member = members
                .get(&spec.name)
                .and_then(|importer| workspace.members().get(importer))
                .expect("a local resolution named a member the resolver found");
            (
                requested.specifier.clone(),
                member.manifest.version().unwrap_or("local").to_string(),
            )
        }
    };

    // The lockfile records what the manifest declares, so the specifier it
    // carries for this request is the one about to be written rather than the
    // seed resolution was given. Were they allowed to differ, every install
    // would find its own lockfile stale and re-resolve a workspace nothing had
    // touched.
    graph
        .importers
        .get_mut(importer)
        .and_then(|resolved| resolved.dependencies.get_mut(&spec.name))
        .expect("the request was either resolved into the target or reused from it")
        .specifier = recorded.clone();

    lockfile::save(&graph, workspace.root())?;

    // Last: never record something that is not already true on disk.
    let mut manifest = Manifest::load(&target.path)?;
    manifest.add_dependency(&spec.name, &recorded);
    manifest.save()?;

    Ok(Installed {
        name: spec.name.clone(),
        version: reported,
    })
}

/// The importers whose recorded specifiers still match their manifests.
///
/// An importer that matches is taken from the lockfile verbatim; one that does
/// not is re-resolved. The comparison is on specifiers rather than on resolved
/// versions because a specifier is what the user wrote, and it is the only
/// thing that can go stale without jerky having done it.
fn reusable_importers(
    locked: Option<&ResolvedGraph>,
    declared: &BTreeMap<ImporterPath, BTreeMap<String, String>>,
    target: &ImporterPath,
    spec: &PackageSpec,
) -> BTreeMap<ImporterPath, Importer> {
    let Some(locked) = locked else {
        return BTreeMap::new();
    };

    declared
        .iter()
        .filter_map(|(path, manifest_declares)| {
            let recorded = locked.importers.get(path)?;
            let fresh = recorded.matches(manifest_declares)
                && (path != target || already_satisfies(recorded, spec));
            fresh.then(|| (path.clone(), recorded.clone()))
        })
        .collect()
}

/// Is the command's own request already answered by what was recorded?
///
/// The manifest can match the lockfile perfectly and still not answer the
/// question being asked — `jerky install lodash@4.18.0` against a lockfile
/// holding 4.17.21 is a new request, not a no-op.
fn already_satisfies(recorded: &Importer, spec: &PackageSpec) -> bool {
    let Some(dependency) = recorded.dependencies.get(&spec.name) else {
        return false;
    };

    match &spec.version {
        // Only the registry can say what `latest` means today, so a bare
        // `jerky install <pkg>` always asks. That is the request, not a
        // shortcoming of the lockfile.
        VersionSpec::Latest => false,
        // Either the request is the specifier verbatim — which is how
        // `workspace:*` matches — or it names the version that was chosen.
        VersionSpec::Exact(requested) => {
            dependency.specifier == *requested
                || matches!(&dependency.resolution, Resolution::Registry(id) if id.version == *requested)
        }
    }
}
