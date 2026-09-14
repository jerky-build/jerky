use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::staging::{STAGING_DIR, StagingDir};

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

/// Give `dst` the mode `src` carries.
///
/// `create_dir_all` takes its mode from the process umask, so a tree recreated
/// in a project would otherwise discard the source's — and the source here is
/// a store entry, whose modes `archive::extract` normalised deliberately. The
/// project would get the umask's answer instead, which under a permissive one
/// is a world-writable directory inside `node_modules`.
///
/// Files need no equivalent: a hard link shares the inode and therefore the
/// mode, and the `EXDEV` copy fallback carries permissions itself.
#[cfg(unix)]
fn reproduce_dir_mode(src: &Path, dst: &Path) -> Result<(), LinkError> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = std::fs::metadata(src)
        .map_err(|source| LinkError::Access {
            path: src.to_path_buf(),
            source,
        })?
        .permissions()
        .mode();

    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode)).map_err(|source| {
        LinkError::Access {
            path: dst.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn reproduce_dir_mode(_src: &Path, _dst: &Path) -> Result<(), LinkError> {
    Ok(())
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
    reproduce_dir_mode(src, dst)?;

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

/// An entry in a `node_modules` that convergence declined to remove, and why.
///
/// Not an error. A half-migrated repository should be told what jerky left
/// where it found it, not stopped — so these travel up through the install's
/// outcome and are printed as warnings, the way a `workspaces` pattern that
/// matched nothing already is.
#[derive(Debug, Clone)]
pub struct Unowned {
    pub path: PathBuf,
    pub reason: UnownedReason,
}

#[derive(Debug, Clone, Copy)]
pub enum UnownedReason {
    /// A real directory or file: a previous `npm install`, or something a
    /// human put there by hand. jerky only ever writes symlinks into an
    /// importer's `node_modules`, so this cannot be jerky's.
    NotASymlink,
    /// A symlink that resolves neither into this workspace's virtual store nor
    /// onto one of its members — someone's `npm link`, most likely. Not ours,
    /// so not ours to remove.
    PointsOutside,
}

/// What convergence is allowed to do with one entry.
enum Ownership {
    /// jerky wrote this link, so jerky may remove it.
    Jerkys,
    /// Someone else's, carrying the reason to report it with.
    Unowned(UnownedReason),
}

/// Remove the links in `importer_dir/node_modules` that `expected` does not
/// account for, and report the entries left alone.
///
/// This is what makes an install convergent rather than additive. A dependency
/// deleted from a `package.json` loses its link, rather than leaving behind a
/// symlink that still resolves — so `require` stops finding a package the
/// project no longer declares, and the tree stops quietly disagreeing with the
/// manifest.
///
/// `expected` holds the names the importer should end up with, spelled as they
/// appear in a manifest: `lodash`, `@types/node`.
///
/// **The ownership test.** An entry is jerky's when it is a symlink whose
/// target, resolved against the link's own parent and normalized lexically,
/// lands either inside `<workspace_root>/node_modules/.jerky` or exactly on one
/// of `members`. Both arms are required, because the two functions that write
/// these links aim at different places: [`symlink_dependency_from`] at the
/// virtual store, [`symlink_local`] at a member's own directory. A test
/// covering only the first would leak every local link, one importer at a time.
///
/// Normalization is lexical rather than [`Path::canonicalize`], consistent with
/// how `relative_path` already compares paths — and, more sharply, because
/// `canonicalize` fails on a dangling link. A link into the virtual store whose
/// entry has since been pruned is jerky's own debris, and an ownership test
/// that could not recognise it would preserve it forever.
///
/// Everything else is left exactly where it was found and returned to the
/// caller. jerky's layout makes ownership provable, which is what lets
/// convergence be total over what jerky manages without ever deleting what
/// another tool or a person put there: the first `jerky install` in a
/// repository that has seen npm must not be a destructive surprise.
pub fn converge(
    importer_dir: &Path,
    workspace_root: &Path,
    members: &BTreeSet<PathBuf>,
    expected: &BTreeSet<String>,
) -> Result<Vec<Unowned>, LinkError> {
    let node_modules = importer_dir.join("node_modules");
    let virtual_store = workspace_root.join("node_modules").join(VIRTUAL_STORE_DIR);
    let mut left_alone = Vec::new();

    for path in entries(&node_modules)? {
        let name = file_name(&path);

        // The virtual store is jerky's, not an importer's dependency, and what
        // belongs in it is a question `prune_virtual_store` answers with the
        // resolved graph rather than with an importer's declared names. It is
        // also a real directory, so falling through would report it as
        // something jerky declined to touch — which would be a lie, and one the
        // root importer sees on every single install.
        if name == VIRTUAL_STORE_DIR {
            continue;
        }

        // A scope is a container, not a package: `@types/node` is one
        // dependency whose link lives two levels down. Descending is what
        // builds the name to compare against `expected`, and it is written now
        // even though nothing scoped installs yet (#22), because the
        // alternative is #22 discovering that convergence silently ignores half
        // the tree.
        //
        // Only a real directory is descended into. A symlink named `@foo` is an
        // ordinary entry, and following one would let a link pointing anywhere
        // at all decide what gets deleted.
        if name.starts_with('@') && is_real_dir(&path)? {
            let mut emptied_it = false;
            for child in entries(&path)? {
                let scoped = format!("{name}/{}", file_name(&child));
                emptied_it |= converge_entry(
                    &child,
                    &scoped,
                    &virtual_store,
                    members,
                    expected,
                    &mut left_alone,
                )?;
            }

            // A scope directory exists only to hold packages, so one this
            // very loop just emptied would otherwise outlive every package it
            // was created for, leaving an empty `@types` where `@types/node`
            // used to be. An entry that was left alone keeps the directory
            // non-empty, which is the answer that wants to be given anyway.
            //
            // `emptied_it` is what keeps this inside the rule the rest of
            // convergence obeys: a scope directory is not a symlink, so the
            // ownership test can say nothing about it, and the only thing that
            // makes removing one provable is having just taken its last
            // package out. An empty `@foo` that was already empty on arrival
            // is someone else's — jerky never creates one it does not
            // immediately fill — so it stays, and removing it would be the one
            // place convergence deleted a real directory it did not write.
            if emptied_it && entries(&path)?.is_empty() {
                std::fs::remove_dir(&path).map_err(|source| LinkError::Access {
                    path: path.clone(),
                    source,
                })?;
            }
            continue;
        }

        converge_entry(
            &path,
            &name,
            &virtual_store,
            members,
            expected,
            &mut left_alone,
        )?;
    }

    Ok(left_alone)
}

/// Decide one `node_modules` entry and act on the answer, reporting whether it
/// was removed.
///
/// The answer is what tells a scope directory whether it is jerky's to take
/// away: an `@types` this loop just emptied is jerky's debris, one that was
/// already empty is someone else's.
///
/// A name the importer still declares is left alone without asking who owns it:
/// the link was just written by this very install, and an *unowned* entry under
/// a declared name never reaches here — `place_symlink` refuses to overwrite
/// anything but a symlink, so a real directory shadowing a declared dependency
/// has already failed the install with `ConflictingEntry`.
fn converge_entry(
    path: &Path,
    name: &str,
    virtual_store: &Path,
    members: &BTreeSet<PathBuf>,
    expected: &BTreeSet<String>,
    left_alone: &mut Vec<Unowned>,
) -> Result<bool, LinkError> {
    if expected.contains(name) {
        return Ok(false);
    }

    match ownership(path, virtual_store, members)? {
        // `remove_file` on a symlink removes the link and never the directory
        // it names, which is the whole reason a dependency can be unlinked from
        // one importer while another goes on using it.
        Ownership::Jerkys => {
            std::fs::remove_file(path).map_err(|source| LinkError::Access {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(true)
        }
        Ownership::Unowned(reason) => {
            left_alone.push(Unowned {
                path: path.to_path_buf(),
                reason,
            });
            Ok(false)
        }
    }
}

/// Can jerky prove it wrote this entry?
fn ownership(
    path: &Path,
    virtual_store: &Path,
    members: &BTreeSet<PathBuf>,
) -> Result<Ownership, LinkError> {
    // symlink_metadata rather than metadata, so a link is judged as a link
    // rather than as whatever it happens to point at — and so a dangling one is
    // seen at all instead of reported as missing.
    let metadata = std::fs::symlink_metadata(path).map_err(|source| LinkError::Access {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_symlink() {
        return Ok(Ownership::Unowned(UnownedReason::NotASymlink));
    }

    let target = std::fs::read_link(path).map_err(|source| LinkError::Access {
        path: path.to_path_buf(),
        source,
    })?;
    let parent = path
        .parent()
        .expect("an entry read out of a directory has that directory as its parent");
    // `join` takes an absolute target as-is, so a hand-written absolute link is
    // judged where it actually points rather than somewhere under the importer.
    let resolved = normalize(&parent.join(target));

    Ok(
        if resolved.starts_with(virtual_store) || members.contains(&resolved) {
            Ownership::Jerkys
        } else {
            Ownership::Unowned(UnownedReason::PointsOutside)
        },
    )
}

/// Resolve `.` and `..` away without touching the filesystem.
///
/// A `..` is only collapsed when there is a plain directory name in front of it
/// to collapse, so climbing past the root leaves the `..` in place rather than
/// inventing a path the link does not name. Such a path then matches neither
/// the virtual store nor a member, which is the honest answer: a link that
/// climbs out of the filesystem is not one jerky wrote.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                _ => normalized.push(Component::ParentDir),
            },
            other => normalized.push(other),
        }
    }
    normalized
}

/// Is this a directory in its own right, rather than a link to one?
fn is_real_dir(path: &Path) -> Result<bool, LinkError> {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_dir())
        .map_err(|source| LinkError::Access {
            path: path.to_path_buf(),
            source,
        })
}

/// Every entry in `dir`, sorted, or none at all when `dir` does not exist.
///
/// A missing directory is the ordinary state of a member that declares nothing
/// and has never been installed into, so it is emptiness rather than a failure.
/// Sorting makes what a run reports a property of the tree rather than of the
/// order the filesystem happened to hand entries back in.
fn entries(dir: &Path) -> Result<Vec<PathBuf>, LinkError> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(LinkError::Access {
                path: dir.to_path_buf(),
                source,
            });
        }
    };

    let mut paths = Vec::new();
    for entry in read {
        let entry = entry.map_err(|source| LinkError::Access {
            path: dir.to_path_buf(),
            source,
        })?;
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .expect("an entry read out of a directory has a final component")
        .to_string_lossy()
        .into_owned()
}

/// Remove the virtual store entries that `expected` does not name.
///
/// `expected` holds the `<name>@<version>` directory names the resolved graph
/// still contains. Everything under `node_modules/.jerky` was written by
/// [`populate_virtual_store`], so unlike an importer's `node_modules` there is
/// no ownership question to ask here — only whether anything still points at
/// it. Without this, deleting a dependency would leave its unpacked tree in the
/// project forever, because nothing else ever removes one.
///
/// The machine-global content store under `~/.jerky/store` is deliberately not
/// touched, and is not even reachable from here: it is shared by every project
/// on the machine, so a project-local convergence has no basis for deciding one
/// of its entries is dead. Collecting it is a separate command (#52).
pub fn prune_virtual_store(
    node_modules: &Path,
    expected: &BTreeSet<String>,
) -> Result<(), LinkError> {
    let virtual_root = node_modules.join(VIRTUAL_STORE_DIR);

    for path in entries(&virtual_root)? {
        let name = file_name(&path);
        // Staging is the linker's own scratch space rather than a package
        // entry. It is emptied by the guard that creates it, and removing the
        // directory itself would only have it recreated on the next install.
        if name == STAGING_DIR {
            continue;
        }

        // A scoped entry is nested, because its directory name contains a
        // `/`: `PackageId`'s `Display` is `{name}@{version}`, so `@types/node`
        // at 20.0.0 is the name `@types/node@20.0.0`, and the `join` in
        // `populate_virtual_store` turns that single name into two levels —
        // `.jerky/@types/node@20.0.0`.
        //
        // So this has to descend for the same reason `converge` does, and the
        // cost of not descending is worse here: a flat read sees `@types`,
        // finds it in no `expected` set (which holds `@types/node@20.0.0`, not
        // `@types`), and takes it for a dead entry — deleting a live store
        // directory and dangling every link into it, silently, on every
        // install. Written now even though nothing scoped resolves yet (#22),
        // because that is exactly the shape of a landmine.
        if name.starts_with('@') && is_real_dir(&path)? {
            for child in entries(&path)? {
                let scoped = format!("{name}/{}", file_name(&child));
                if expected.contains(&scoped) {
                    continue;
                }
                remove_entry(&child)?;
            }

            // Unlike a scope directory in an importer's `node_modules`, an
            // empty one here needs no proof of ownership and so is removed
            // whether or not this loop is what emptied it: everything under
            // `.jerky` was written by `populate_virtual_store`, so ownership
            // is settled by location rather than by a link target.
            if entries(&path)?.is_empty() {
                std::fs::remove_dir(&path).map_err(|source| LinkError::Access {
                    path: path.clone(),
                    source,
                })?;
            }
            continue;
        }

        if expected.contains(&name) {
            continue;
        }

        remove_entry(&path)?;
    }

    Ok(())
}

/// Remove one virtual store entry, whatever kind of thing it turned out to be.
///
/// Branching on the type rather than reaching straight for `remove_dir_all`,
/// which fails on anything that is not a directory. Nothing should ever have
/// put a file here, and failing an otherwise good install over one would be a
/// poor trade.
fn remove_entry(path: &Path) -> Result<(), LinkError> {
    let removed = if is_real_dir(path)? {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    removed.map_err(|source| LinkError::Access {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
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
    #[cfg(unix)]
    fn hard_link_tree_reproduces_directory_modes() {
        // `create_dir_all` takes its mode from the process umask, so a tree
        // recreated in a project would otherwise discard whatever the store
        // holds — and the store's modes are the normalised ones extraction
        // chose. The project would get the umask's answer instead, which under
        // a permissive one is a world-writable directory inside `node_modules`.
        //
        // 0o700 rather than 0o755 on purpose: no ordinary umask produces it,
        // so this fails if the mode is merely defaulted rather than carried.
        use crate::testing::mode_of;

        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        std::fs::set_permissions(src.join("lib"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let dst = root.path().join("dst");

        hard_link_tree(&src, &dst).unwrap();

        assert_eq!(mode_of(&dst.join("lib")), 0o700);
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

    /// Does anything at all still sit at `path`? `exists` follows links and so
    /// answers `false` for a dangling one, which is precisely the entry these
    /// tests are about.
    fn still_there(path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    fn names(expected: &[&str]) -> BTreeSet<String> {
        expected.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn a_dangling_link_into_the_virtual_store_is_recognised_as_ours_and_removed() {
        // The reason ownership is decided lexically rather than by
        // `canonicalize`, which fails outright on a link that resolves nowhere.
        // Taking that failure as "not jerky's" would make a broken link of
        // jerky's own making permanent, and pruning the virtual store is what
        // creates one.
        let root = TempDir::new().unwrap();
        let workspace_root = root.path().join("ws");
        let node_modules = workspace_root.join("node_modules");
        std::fs::create_dir_all(&node_modules).unwrap();
        std::os::unix::fs::symlink(
            ".jerky/lodash@4.17.21/node_modules/lodash",
            node_modules.join("lodash"),
        )
        .unwrap();

        let left_alone = converge(
            &workspace_root,
            &workspace_root,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();

        assert!(
            left_alone.is_empty(),
            "a link jerky wrote was reported as someone else's"
        );
        assert!(
            !still_there(&node_modules.join("lodash")),
            "the dangling link survived"
        );
    }

    #[test]
    fn a_link_to_a_workspace_member_is_ours() {
        // `symlink_local` points at a member directory, not into `.jerky`. An
        // ownership test covering only the virtual store would leak every local
        // link, one importer at a time — and it would do so silently, since a
        // local link always resolves.
        let root = TempDir::new().unwrap();
        let workspace_root = root.path().join("ws");
        let importer_dir = workspace_root.join("apps/web");
        let member_dir = workspace_root.join("packages/ui");
        std::fs::create_dir_all(&importer_dir).unwrap();
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(member_dir.join("package.json"), r#"{"name":"ui"}"#).unwrap();

        symlink_local(&importer_dir, "ui", &member_dir).unwrap();
        let members = BTreeSet::from([
            workspace_root.clone(),
            importer_dir.clone(),
            member_dir.clone(),
        ]);

        let left_alone =
            converge(&importer_dir, &workspace_root, &members, &BTreeSet::new()).unwrap();

        assert!(
            left_alone.is_empty(),
            "a link at a workspace member read as unowned"
        );
        assert!(
            !still_there(&importer_dir.join("node_modules/ui")),
            "a link at a workspace member survived convergence"
        );
        assert!(
            member_dir.join("package.json").is_file(),
            "unlinking a member deleted the member"
        );
    }

    #[test]
    fn an_emptied_scope_directory_is_removed() {
        // Removing `node_modules/@types/node` must not leave an empty `@types`
        // behind. The contrast is the other half: a scope is descended into
        // rather than judged as a unit, so one that still holds a declared
        // package stays exactly where it is.
        let root = TempDir::new().unwrap();
        let workspace_root = root.path().join("ws");
        let node_modules = workspace_root.join("node_modules");
        for scope in ["@types", "@scope"] {
            std::fs::create_dir_all(node_modules.join(scope)).unwrap();
        }
        // One climb, because the link sits inside the scope directory rather
        // than directly in `node_modules`.
        std::os::unix::fs::symlink(
            "../.jerky/@types/node@20.0.0/node_modules/@types/node",
            node_modules.join("@types/node"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../.jerky/@scope/live@1.0.0/node_modules/@scope/live",
            node_modules.join("@scope/live"),
        )
        .unwrap();

        let left_alone = converge(
            &workspace_root,
            &workspace_root,
            &BTreeSet::new(),
            &names(&["@scope/live"]),
        )
        .unwrap();

        assert!(left_alone.is_empty());
        assert!(
            !still_there(&node_modules.join("@types")),
            "the scope outlived every package that was ever in it"
        );
        assert!(
            still_there(&node_modules.join("@scope/live")),
            "a declared scoped package lost its link"
        );
    }

    #[test]
    fn the_virtual_store_is_pruned_to_the_graph() {
        // `.jerky` is not judged by the ownership test at all — everything in
        // it was written by `populate_virtual_store` — so what governs is
        // whether the graph still names the entry.
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let node_modules = root.path().join("ws").join("node_modules");
        populate_virtual_store(&src, &node_modules, "lodash@4.17.21", "lodash").unwrap();
        populate_virtual_store(&src, &node_modules, "lodash@3.10.1", "lodash").unwrap();

        prune_virtual_store(&node_modules, &names(&["lodash@4.17.21"])).unwrap();

        assert!(node_modules.join(".jerky/lodash@4.17.21").is_dir());
        assert!(
            !still_there(&node_modules.join(".jerky/lodash@3.10.1")),
            "an entry the graph no longer contains stayed unpacked in the project"
        );
        // Staging is the linker's own scratch space, recreated on the next
        // install, so pruning has no business deciding it is dead.
        assert!(node_modules.join(".jerky/.staging").is_dir());
    }

    #[test]
    fn a_scoped_store_entry_the_graph_still_names_survives_the_prune() {
        // A scoped entry's directory name contains a `/`, so it is nested two
        // levels deep — `.jerky/@types/node@20.0.0`. A prune that read one
        // level would see `@types`, find it in no `expected` set, and
        // `remove_dir_all` a live entry, dangling every link into it silently
        // on every install.
        // Built directly rather than through `populate_virtual_store`, which
        // cannot create a scoped entry today: it renames staging onto
        // `.jerky/@types/node@20.0.0` without creating the `@types` level
        // first, so the rename fails with `ENOENT`. That is #22's to fix —
        // what is under test here is that the prune agrees with the layout
        // `populate_virtual_store` names, whenever it can write it.
        let root = TempDir::new().unwrap();
        let node_modules = root.path().join("ws").join("node_modules");
        for version in ["20.0.0", "18.0.0"] {
            std::fs::create_dir_all(
                node_modules
                    .join(".jerky")
                    .join("@types")
                    .join(format!("node@{version}"))
                    .join("node_modules")
                    .join("@types")
                    .join("node"),
            )
            .unwrap();
        }

        prune_virtual_store(&node_modules, &names(&["@types/node@20.0.0"])).unwrap();

        assert!(
            node_modules.join(".jerky/@types/node@20.0.0").is_dir(),
            "the graph still names this entry and the prune deleted it anyway"
        );
        assert!(
            !still_there(&node_modules.join(".jerky/@types/node@18.0.0")),
            "a scoped entry the graph dropped stayed unpacked"
        );
        assert!(
            node_modules.join(".jerky/@types").is_dir(),
            "the scope still holds a live entry"
        );
    }

    #[test]
    fn a_scope_emptied_by_the_prune_goes_with_its_last_entry() {
        let root = TempDir::new().unwrap();
        let node_modules = root.path().join("ws").join("node_modules");
        std::fs::create_dir_all(node_modules.join(".jerky/@types/node@20.0.0")).unwrap();

        prune_virtual_store(&node_modules, &BTreeSet::new()).unwrap();

        assert!(
            !still_there(&node_modules.join(".jerky/@types")),
            "the scope outlived the only entry it was created for"
        );
    }

    #[test]
    fn a_scope_directory_convergence_did_not_empty_is_left_alone() {
        // The one place `converge` would otherwise remove something that is
        // not a symlink. An empty `@foo` that was already empty on arrival is
        // not jerky's — jerky never creates a scope directory it does not
        // immediately fill — and deleting it would contradict the rule the
        // rest of convergence obeys: remove only what jerky can prove it
        // wrote.
        let root = TempDir::new().unwrap();
        let workspace_root = root.path().join("ws");
        let node_modules = workspace_root.join("node_modules");
        std::fs::create_dir_all(node_modules.join("@someone-elses")).unwrap();

        let left_alone = converge(
            &workspace_root,
            &workspace_root,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();

        assert!(
            still_there(&node_modules.join("@someone-elses")),
            "convergence deleted a real directory it never wrote"
        );
        // Not reported either: it holds no dependency, so there is nothing to
        // tell the user jerky declined to touch.
        assert!(left_alone.is_empty());
    }
}
