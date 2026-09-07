use std::path::Path;

use thiserror::Error;

use crate::archive::{self, ArchiveError};
use crate::cli::PackageSpec;
use crate::integrity::IntegrityError;
use crate::linker::{self, LinkError};
use crate::manifest::{Manifest, ManifestError};
use crate::registry::{RegistryClient, RegistryError};
use crate::store::{Store, StoreError};

#[derive(Debug, Error)]
pub enum InstallError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
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
}

#[derive(Debug, Clone)]
pub struct Installed {
    pub name: String,
    pub version: String,
}

/// Install one package into `project_dir`.
///
/// Ordering is deliberate. The manifest write is last, so a failure anywhere
/// leaves at worst an installed-but-unrecorded package — harmless and
/// self-healing on rerun — rather than a `package.json` claiming a dependency
/// that is not on disk.
pub fn install(
    project_dir: &Path,
    store: &Store,
    registry: &dyn RegistryClient,
    spec: &PackageSpec,
) -> Result<Installed, InstallError> {
    // Load the manifest first: failing before any network or disk work is the
    // cheapest possible way to reject a project with no package.json.
    let mut manifest = Manifest::load(project_dir)?;

    let metadata = registry.version_metadata(&spec.name, spec.version.as_request())?;
    let name = metadata.name.clone();
    let version = metadata.version.clone();

    let integrity = metadata
        .dist
        .integrity()
        .map_err(|source| InstallError::Integrity {
            name: name.clone(),
            version: version.clone(),
            source,
        })?;
    let key = integrity.store_key();

    // Populating is skipped entirely when the store already holds these bytes,
    // so a repeat install performs no download.
    let entry = if store.contains(&key) {
        store.entry_path(&key)
    } else {
        let tarball = registry.fetch_tarball(&metadata.dist.tarball)?;

        // Verify the complete buffer before a single byte is extracted. A
        // stream-and-hash design would only detect a mismatch after writing
        // attacker-controlled files to disk.
        integrity
            .verify(&tarball)
            .map_err(|source| InstallError::Integrity {
                name: name.clone(),
                version: version.clone(),
                source,
            })?;

        store.commit(&key, |staging| archive::extract(&tarball, staging))?
    };

    let node_modules = project_dir.join("node_modules");
    let dir_name = format!("{name}@{version}");
    linker::populate_virtual_store(&entry, &node_modules, &dir_name, &name)?;
    linker::symlink_dependency(&node_modules, &name, &dir_name)?;

    // Last: never record something that is not already true on disk. And
    // always the concrete version from the response, never a caret range that
    // spec 1 has no resolver to honour.
    manifest.add_dependency(&name, &version);
    manifest.save()?;

    Ok(Installed { name, version })
}
