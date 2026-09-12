use std::collections::BTreeMap;
use std::path::PathBuf;

use thiserror::Error;

use crate::archive::{self, ArchiveError};
use crate::cli::PackageSpec;
use crate::integrity::IntegrityError;
use crate::linker::{self, LinkError};
use crate::lockfile::{self, LockfileError};
use crate::manifest::{Manifest, ManifestError};
use crate::registry::{RegistryClient, RegistryError};
use crate::resolver::{self, ImporterPath, PackageId, Resolution, ResolveError};
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
    let mut roots: BTreeMap<ImporterPath, BTreeMap<String, String>> = workspace
        .members()
        .iter()
        .map(|(path, member)| (path.clone(), member.manifest.dependencies()))
        .collect();
    roots
        .get_mut(importer)
        .expect("the target importer is a member, checked above")
        .insert(spec.name.clone(), spec.version.as_request().to_string());

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

    let graph = resolver::resolve(registry, &roots, &members)?;

    // The virtual store sits at the workspace root, which is the whole reason
    // two importers on one version share an entry rather than each unpacking
    // their own.
    let node_modules = workspace.root().join("node_modules");
    let mut entries: BTreeMap<PackageId, PathBuf> = BTreeMap::new();

    for (id, package) in &graph.packages {
        let key = package.integrity.store_key();

        // Populating is skipped entirely when the store already holds these
        // bytes, so a repeat install performs no download.
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

    lockfile::save(&graph, workspace.root())?;

    // Last: never record something that is not already true on disk.
    let requested = &graph.importers[importer].dependencies[&spec.name];
    let (recorded, reported) = match &requested.resolution {
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

    let mut manifest = Manifest::load(&target.path)?;
    manifest.add_dependency(&spec.name, &recorded);
    manifest.save()?;

    Ok(Installed {
        name: spec.name.clone(),
        version: reported,
    })
}
