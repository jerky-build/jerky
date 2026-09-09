use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::staging::StagingDir;

/// `EXDEV`, "cross-device link". Both Linux and macOS use 18.
/// `io::ErrorKind::CrossesDevices` would be cleaner but is still unstable.
const EXDEV: i32 = 18;

const VIRTUAL_STORE_DIR: &str = ".jerky";

#[derive(Debug, Error)]
pub enum LinkError {
    #[error("failed to link {from} to {to}")]
    Io {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to access {path}")]
    Access {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} already exists and is not a symlink jerky can replace")]
    ConflictingEntry { path: PathBuf },
}

/// Hard-link one file, falling back to a copy across filesystems.
///
/// The store and a project can legitimately live on different volumes, where
/// `hard_link` fails with `EXDEV`. Only that specific errno falls back — any
/// other failure is a real error, not something to paper over with a copy.
pub fn link_file(src: &Path, dst: &Path) -> Result<(), LinkError> {
    match std::fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(source) if source.raw_os_error() == Some(EXDEV) => std::fs::copy(src, dst)
            .map(|_| ())
            .map_err(|source| LinkError::Io {
                from: src.to_path_buf(),
                to: dst.to_path_buf(),
                source,
            }),
        Err(source) => Err(LinkError::Io {
            from: src.to_path_buf(),
            to: dst.to_path_buf(),
            source,
        }),
    }
}

fn walk_tree(
    src: &Path,
    dst: &Path,
    place: &dyn Fn(&Path, &Path) -> Result<(), LinkError>,
) -> Result<(), LinkError> {
    std::fs::create_dir_all(dst).map_err(|source| LinkError::Access {
        path: dst.to_path_buf(),
        source,
    })?;

    let entries = std::fs::read_dir(src).map_err(|source| LinkError::Access {
        path: src.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| LinkError::Access {
            path: src.to_path_buf(),
            source,
        })?;
        let from = entry.path();
        let to = dst.join(entry.file_name());

        let file_type = entry.file_type().map_err(|source| LinkError::Access {
            path: from.clone(),
            source,
        })?;

        if file_type.is_dir() {
            walk_tree(&from, &to, place)?;
        } else {
            place(&from, &to)?;
        }
    }

    Ok(())
}

/// Recreate `src`'s directory structure at `dst`, hard-linking every file.
///
/// Directories cannot be hard-linked, so they are created and only regular
/// files are linked.
pub fn hard_link_tree(src: &Path, dst: &Path) -> Result<(), LinkError> {
    walk_tree(src, dst, &link_file)
}

/// Recreate `src` at `dst` by copying. The `EXDEV` fallback path, exposed so
/// it can be tested directly — CI cannot practically mount a second
/// filesystem to trigger it naturally.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<(), LinkError> {
    walk_tree(src, dst, &|from: &Path, to: &Path| {
        std::fs::copy(from, to)
            .map(|_| ())
            .map_err(|source| LinkError::Io {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                source,
            })
    })
}

/// Build `node_modules/.jerky/<dir_name>/node_modules/<pkg_name>/` from a
/// store entry, and return the `<dir_name>` directory.
///
/// The doubled `node_modules` is the mechanism, not an accident: Node resolves
/// a package's dependencies by walking up from its directory looking for a
/// `node_modules`, so a package nested inside its own sees exactly its
/// declared dependencies as siblings and nothing else.
///
/// Uses the same stage-and-rename discipline as the store, for the same
/// reason: a half-linked tree looks present.
pub fn populate_virtual_store(
    store_entry: &Path,
    node_modules: &Path,
    dir_name: &str,
    pkg_name: &str,
) -> Result<PathBuf, LinkError> {
    let virtual_root = node_modules.join(VIRTUAL_STORE_DIR);
    let target = virtual_root.join(dir_name);
    if target.is_dir() {
        return Ok(target);
    }

    // The guard deletes the staging tree on any exit that is not an explicit
    // keep — including a panic partway through linking, which the previous
    // hand-rolled unwind here could not cover.
    let mut staging = StagingDir::create_under(&virtual_root).map_err(|err| LinkError::Access {
        path: err.path,
        source: err.source,
    })?;

    hard_link_tree(
        store_entry,
        &staging.path().join("node_modules").join(pkg_name),
    )?;

    let staged = staging.keep();
    match std::fs::rename(&staged, &target) {
        Ok(()) => Ok(target),
        Err(source) => {
            let _ = std::fs::remove_dir_all(&staged);
            if target.is_dir() {
                Ok(target)
            } else {
                Err(LinkError::Io {
                    from: staged,
                    to: target,
                    source,
                })
            }
        }
    }
}

/// Link `node_modules/<pkg_name>` at the package's virtual store directory.
///
/// The target is relative so that moving or copying a project does not break
/// every link. It is resolved relative to `node_modules/` — the directory
/// holding the link — so it carries no leading `../`.
pub fn symlink_dependency(
    node_modules: &Path,
    pkg_name: &str,
    dir_name: &str,
) -> Result<(), LinkError> {
    std::fs::create_dir_all(node_modules).map_err(|source| LinkError::Access {
        path: node_modules.to_path_buf(),
        source,
    })?;

    let link = node_modules.join(pkg_name);
    let target = Path::new(VIRTUAL_STORE_DIR)
        .join(dir_name)
        .join("node_modules")
        .join(pkg_name);

    // symlink_metadata does not follow the link, so a dangling link is still
    // detected and replaced rather than reported as missing.
    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::remove_file(&link).map_err(|source| LinkError::Access {
                path: link.clone(),
                source,
            })?;
        }
        Ok(_) => return Err(LinkError::ConflictingEntry { path: link }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(LinkError::Access { path: link, source }),
    }

    std::os::unix::fs::symlink(&target, &link).map_err(|source| LinkError::Io {
        from: target,
        to: link,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;
    use tempfile::TempDir;

    fn store_entry(root: &Path) -> PathBuf {
        let entry = root.join("store-entry");
        std::fs::create_dir_all(entry.join("lib")).unwrap();
        std::fs::write(entry.join("package.json"), r#"{"name":"lodash"}"#).unwrap();
        std::fs::write(entry.join("lib/core.js"), "core").unwrap();
        entry
    }

    #[test]
    fn hard_link_tree_shares_inodes_rather_than_copying() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let dst = root.path().join("dst");

        hard_link_tree(&src, &dst).unwrap();

        // The whole point of the store: one copy of the bytes, many names.
        // A silent regression to fs::copy passes every other assertion here.
        for relative in ["package.json", "lib/core.js"] {
            let a = std::fs::metadata(src.join(relative)).unwrap();
            let b = std::fs::metadata(dst.join(relative)).unwrap();
            assert_eq!(
                (a.dev(), a.ino()),
                (b.dev(), b.ino()),
                "{relative} was copied, not hard-linked"
            );
        }
    }

    #[test]
    fn hard_link_tree_recreates_nested_directories() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let dst = root.path().join("dst");

        hard_link_tree(&src, &dst).unwrap();

        assert!(dst.join("lib").is_dir());
        assert_eq!(
            std::fs::read_to_string(dst.join("lib/core.js")).unwrap(),
            "core"
        );
    }

    #[test]
    fn copy_tree_duplicates_contents_with_distinct_inodes() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let dst = root.path().join("dst");

        copy_tree(&src, &dst).unwrap();

        let a = std::fs::metadata(src.join("package.json")).unwrap();
        let b = std::fs::metadata(dst.join("package.json")).unwrap();
        assert_ne!((a.dev(), a.ino()), (b.dev(), b.ino()));
        assert_eq!(
            std::fs::read_to_string(dst.join("package.json")).unwrap(),
            r#"{"name":"lodash"}"#
        );
    }

    #[test]
    fn populate_virtual_store_nests_the_package_under_its_own_node_modules() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let node_modules = root.path().join("node_modules");

        let dir = populate_virtual_store(&src, &node_modules, "lodash@4.17.21", "lodash").unwrap();

        assert_eq!(dir, node_modules.join(".jerky").join("lodash@4.17.21"));
        assert!(dir.join("node_modules/lodash/package.json").is_file());
    }

    #[test]
    fn populate_virtual_store_leaves_no_staging_debris() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let node_modules = root.path().join("node_modules");

        populate_virtual_store(&src, &node_modules, "lodash@4.17.21", "lodash").unwrap();

        let staging = node_modules.join(".jerky").join(".staging");
        if staging.exists() {
            assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
        }
    }

    #[test]
    fn symlink_dependency_creates_a_relative_link_that_resolves() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let node_modules = root.path().join("node_modules");
        populate_virtual_store(&src, &node_modules, "lodash@4.17.21", "lodash").unwrap();

        symlink_dependency(&node_modules, "lodash", "lodash@4.17.21").unwrap();

        let link = node_modules.join("lodash");
        let target = std::fs::read_link(&link).unwrap();
        assert!(
            target.is_relative(),
            "an absolute target breaks as soon as the project moves"
        );
        assert_eq!(
            target,
            Path::new(".jerky/lodash@4.17.21/node_modules/lodash")
        );
        // And it actually resolves.
        assert!(link.join("package.json").is_file());
    }

    #[test]
    fn symlink_dependency_replaces_an_existing_link() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let node_modules = root.path().join("node_modules");
        populate_virtual_store(&src, &node_modules, "lodash@4.17.21", "lodash").unwrap();
        populate_virtual_store(&src, &node_modules, "lodash@3.0.0", "lodash").unwrap();

        symlink_dependency(&node_modules, "lodash", "lodash@3.0.0").unwrap();
        symlink_dependency(&node_modules, "lodash", "lodash@4.17.21").unwrap();

        let target = std::fs::read_link(node_modules.join("lodash")).unwrap();
        assert_eq!(
            target,
            Path::new(".jerky/lodash@4.17.21/node_modules/lodash")
        );
    }
}
