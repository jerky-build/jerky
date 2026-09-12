use std::path::{Component, Path, PathBuf};

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

/// The relative path from the directory `from` to `to`.
///
/// This is the only place in the module that writes a `..`. Every link target
/// is built from it, so the climb falls out of where the two paths diverge
/// rather than being assumed — which is what stops a link in a nested importer
/// from silently getting a root-shaped target.
///
/// Both paths are absolute, so they share at least the filesystem root and the
/// common prefix is never empty. The comparison is purely lexical: callers
/// must pass paths built from a common base, because a canonicalized root and
/// an uncanonicalized importer diverge at the first symlinked component and
/// would silently produce a climb to the wrong place rather than an error.
fn relative_path(from: &Path, to: &Path) -> PathBuf {
    debug_assert!(
        from.is_absolute() && to.is_absolute(),
        "relative_path compares component by component, which only means anything for absolute paths"
    );

    let mut from = from.components().peekable();
    let mut to = to.components().peekable();
    while from.peek().is_some() && from.peek() == to.peek() {
        from.next();
        to.next();
    }

    let mut relative = PathBuf::new();
    for _ in from {
        relative.push(Component::ParentDir);
    }
    relative.extend(to);
    relative
}

/// Create `link` pointing at `target`, replacing a link jerky already wrote.
fn place_symlink(link: PathBuf, target: PathBuf) -> Result<(), LinkError> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|source| LinkError::Access {
            path: parent.to_path_buf(),
            source,
        })?;
    }

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

/// Place a link named `pkg_name` inside `link_dir`, pointing at `entry`.
///
/// The three public link functions differ only in which absolute path they
/// name as `entry`. Everything after that — deriving the climb, creating the
/// directory, replacing a link jerky already wrote — is the same work, so it
/// lives here rather than being repeated with one line changed.
fn link_at(link_dir: &Path, pkg_name: &str, entry: &Path) -> Result<(), LinkError> {
    let target = relative_path(link_dir, entry);
    place_symlink(link_dir.join(pkg_name), target)
}

/// Link `<importer_dir>/node_modules/<pkg_name>` straight at a workspace
/// member's own directory.
///
/// A local dependency has no store entry, because there is no tarball: the
/// bytes are already in the repo and there is nothing to verify or unpack.
/// The link therefore points at the member itself rather than into the virtual
/// store, and edits to that member are visible to its dependents immediately.
pub fn symlink_local(
    importer_dir: &Path,
    pkg_name: &str,
    target_dir: &Path,
) -> Result<(), LinkError> {
    link_at(&importer_dir.join("node_modules"), pkg_name, target_dir)
}

/// Link one store entry at another's private `node_modules`.
///
/// `owner_entry` is what [`populate_virtual_store`] returned for the depending
/// package, so the link lands inside that package's own `node_modules` and
/// points at a sibling in the same store:
/// `.jerky/b@1.0.0/node_modules/d -> ../../d@1.5.0/node_modules/d`. Climbing
/// out and back down is what keeps the target inside the store rather than
/// reaching into an importer's `node_modules`, so a package sees exactly the
/// dependencies it declared and nothing a sibling happens to have installed.
pub fn symlink_into_store(
    owner_entry: &Path,
    pkg_name: &str,
    dir_name: &str,
) -> Result<(), LinkError> {
    let link_dir = owner_entry.join("node_modules");
    let virtual_store = owner_entry
        .parent()
        .expect("a store entry is a directory inside the virtual store, so it has a parent");
    let entry = virtual_store
        .join(dir_name)
        .join("node_modules")
        .join(pkg_name);

    link_at(&link_dir, pkg_name, &entry)
}

/// Link `<importer_dir>/node_modules/<pkg_name>` at the package's directory in
/// the workspace root's virtual store.
///
/// The target is relative so that moving or copying a workspace does not break
/// every link, and its shape depends on where the importer sits: a link in the
/// root's `node_modules` climbs nowhere, while one in `packages/ui/node_modules`
/// climbs three levels to reach the root's store. A workspace of one is not a
/// special case here — it is an importer at depth zero, and takes this path
/// like any other.
pub fn symlink_dependency_from(
    importer_dir: &Path,
    workspace_root: &Path,
    pkg_name: &str,
    dir_name: &str,
) -> Result<(), LinkError> {
    debug_assert!(
        importer_dir.starts_with(workspace_root),
        "an importer outside its own workspace would be handed a climb that escapes the root"
    );

    let entry = workspace_root
        .join("node_modules")
        .join(VIRTUAL_STORE_DIR)
        .join(dir_name)
        .join("node_modules")
        .join(pkg_name);

    link_at(&importer_dir.join("node_modules"), pkg_name, &entry)
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

        symlink_dependency_from(root.path(), root.path(), "lodash", "lodash@4.17.21").unwrap();

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

        symlink_dependency_from(root.path(), root.path(), "lodash", "lodash@3.0.0").unwrap();
        symlink_dependency_from(root.path(), root.path(), "lodash", "lodash@4.17.21").unwrap();

        let target = std::fs::read_link(node_modules.join("lodash")).unwrap();
        assert_eq!(
            target,
            Path::new(".jerky/lodash@4.17.21/node_modules/lodash")
        );
    }

    /// A workspace root with a populated virtual store, and the absolute path
    /// to an importer's directory inside it.
    fn workspace(root: &Path, importer: &str, dir_name: &str) -> (PathBuf, PathBuf) {
        let src = store_entry(root);
        let workspace_root = root.join("ws");
        populate_virtual_store(
            &src,
            &workspace_root.join("node_modules"),
            dir_name,
            "lodash",
        )
        .unwrap();

        let importer_dir = if importer == "." {
            workspace_root.clone()
        } else {
            workspace_root.join(importer)
        };
        std::fs::create_dir_all(&importer_dir).unwrap();
        (workspace_root, importer_dir)
    }

    #[test]
    fn a_root_importer_link_climbs_nowhere() {
        let root = TempDir::new().unwrap();
        let (workspace_root, importer_dir) = workspace(root.path(), ".", "lodash@4.17.21");

        symlink_dependency_from(&importer_dir, &workspace_root, "lodash", "lodash@4.17.21")
            .unwrap();

        let link = importer_dir.join("node_modules").join("lodash");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new(".jerky/lodash@4.17.21/node_modules/lodash")
        );
        assert!(
            link.join("package.json").is_file(),
            "the link does not resolve"
        );
    }

    #[test]
    fn depth_is_computed_not_assumed() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let workspace_root = root.path().join("ws");
        let store = workspace_root.join("node_modules");
        populate_virtual_store(&src, &store, "lodash@4.17.21", "lodash").unwrap();
        populate_virtual_store(&src, &store, "lodash@4.18.0", "lodash").unwrap();

        // The same package, linked from importers at two different depths,
        // must get two different targets and both must resolve.
        for (importer, dir_name, expected) in [
            (
                "packages/ui",
                "lodash@4.17.21",
                "../../../node_modules/.jerky/lodash@4.17.21/node_modules/lodash",
            ),
            (
                "apps/web",
                "lodash@4.18.0",
                "../../../node_modules/.jerky/lodash@4.18.0/node_modules/lodash",
            ),
        ] {
            let importer_dir = workspace_root.join(importer);
            std::fs::create_dir_all(&importer_dir).unwrap();

            symlink_dependency_from(&importer_dir, &workspace_root, "lodash", dir_name).unwrap();

            let link = importer_dir.join("node_modules").join("lodash");
            assert_eq!(
                std::fs::read_link(&link).unwrap(),
                Path::new(expected),
                "{importer} got a target shaped for another depth"
            );
            assert!(
                link.join("package.json").is_file(),
                "{importer}'s link reads correctly but does not resolve"
            );
        }
    }

    #[test]
    fn a_one_level_importer_climbs_twice() {
        let root = TempDir::new().unwrap();
        let (workspace_root, importer_dir) = workspace(root.path(), "ui", "lodash@4.17.21");

        symlink_dependency_from(&importer_dir, &workspace_root, "lodash", "lodash@4.17.21")
            .unwrap();

        let link = importer_dir.join("node_modules").join("lodash");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("../../node_modules/.jerky/lodash@4.17.21/node_modules/lodash")
        );
        assert!(
            link.join("package.json").is_file(),
            "the link does not resolve"
        );
    }

    #[test]
    fn an_intra_store_link_climbs_out_of_its_own_node_modules() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let node_modules = root.path().join("ws").join("node_modules");
        let owner = populate_virtual_store(&src, &node_modules, "b@1.0.0", "b").unwrap();
        populate_virtual_store(&src, &node_modules, "d@1.5.0", "d").unwrap();

        symlink_into_store(&owner, "d", "d@1.5.0").unwrap();

        let link = owner.join("node_modules").join("d");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("../../d@1.5.0/node_modules/d")
        );
        assert!(
            link.join("package.json").is_file(),
            "the link does not resolve"
        );
    }

    #[test]
    fn a_local_package_links_straight_at_its_directory() {
        let root = TempDir::new().unwrap();
        let workspace_root = root.path().join("ws");
        let importer_dir = workspace_root.join("apps/web");
        let member_dir = workspace_root.join("packages/ui");
        std::fs::create_dir_all(&importer_dir).unwrap();
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(member_dir.join("package.json"), r#"{"name":"ui"}"#).unwrap();

        symlink_local(&importer_dir, "ui", &member_dir).unwrap();

        let link = importer_dir.join("node_modules").join("ui");
        // Straight at the member, not into the virtual store: there is no
        // store entry, because there is no tarball.
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("../../../packages/ui")
        );
        assert!(
            link.join("package.json").is_file(),
            "the link does not resolve"
        );
    }

    #[test]
    fn a_local_link_from_the_root_importer_needs_one_climb() {
        let root = TempDir::new().unwrap();
        let workspace_root = root.path().join("ws");
        let member_dir = workspace_root.join("packages/ui");
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(member_dir.join("package.json"), r#"{"name":"ui"}"#).unwrap();

        symlink_local(&workspace_root, "ui", &member_dir).unwrap();

        let link = workspace_root.join("node_modules").join("ui");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("../packages/ui")
        );
        assert!(
            link.join("package.json").is_file(),
            "the link does not resolve"
        );
    }
}
