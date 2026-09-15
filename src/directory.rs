//! Directories jerky creates, at a mode jerky chose.
//!
//! `std::fs::create_dir` and `create_dir_all` take their mode from the process
//! umask, which is the ambient setting of whoever started the process rather
//! than a decision about what the directory is for. Under a permissive umask —
//! 0o002 or 0o000, ordinary on a developer box and on CI runners — every
//! directory they make is group- or world-writable.
//!
//! That matters because of where jerky's directories are. The store and the
//! metadata cache are `$HOME`-wide and shared by every project on the machine,
//! and the store is hard-linked into each of them, so a directory another user
//! can write into is one they can add or replace a file inside a package every
//! project imports. A scope directory in an importer's `node_modules` is the
//! same hole one level down: write access to it is permission to add a
//! package there. So every directory jerky creates is `0o755`, and this module
//! is the one place that says so.

use std::io;
use std::path::{Path, PathBuf};

/// The mode every directory jerky creates lands with: traversable and
/// readable by anyone, writable only by its owner.
const MODE: u32 = 0o755;

/// Create `dir` at `0o755`, failing if it is already there.
///
/// The `create_dir` of this module. Reach for it when the caller needs to know
/// it made the directory rather than found it — a staging name, which two
/// processes must not end up sharing.
pub(crate) fn create(dir: &Path) -> io::Result<()> {
    std::fs::create_dir(dir)?;
    set_mode(dir)
}

/// Create `dir` and every level of it that did not exist, at `0o755`.
///
/// The `create_dir_all` of this module, and the one most callers reach for.
///
/// Only the levels that were actually missing are chmodded. A level that was
/// already there belongs to whoever made it, and rewriting its mode would be
/// this deciding something about a directory it did not create. It is also
/// what keeps the extractor cheap: from its second entry onwards a tarball's
/// parent directories all exist, so the common call is answered by one stat
/// instead of a chmod per ancestor per entry — which on a 93k-file tree is
/// several hundred thousand syscalls that change nothing.
pub(crate) fn create_all(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }

    let missing: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|level| !level.exists())
        .map(Path::to_path_buf)
        .collect();

    std::fs::create_dir_all(dir)?;

    for level in missing {
        set_mode(&level)?;
    }
    Ok(())
}

fn set_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(MODE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use tempfile::TempDir;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn every_level_it_creates_is_0o755() {
        // Several levels below one that exists, because the hole this guards
        // is in the levels `create_dir_all` fills in on the way down: a test
        // that only ever creates a leaf proves nothing about them.
        //
        // Under a strict umask this holds without the rule, so it bites only
        // where the hole is real. The `umask 0` run of the suite is what makes
        // it bite.
        let dir = TempDir::new().unwrap();
        let deepest = dir.path().join("a/b/c");

        create_all(&deepest).unwrap();

        for level in [dir.path().join("a"), dir.path().join("a/b"), deepest] {
            assert_eq!(mode_of(&level), 0o755, "{} is too open", level.display());
        }
    }

    #[test]
    fn a_level_that_already_existed_keeps_its_own_mode() {
        // A directory this did not create belongs to whoever did, and
        // rewriting its mode would be jerky deciding something about a
        // directory it does not own. It is also what keeps extraction cheap:
        // the parent of every entry in a 93k-file tarball already exists by
        // the second entry, and re-asserting the mode of each of its ancestors
        // per entry is several hundred thousand syscalls that change nothing.
        //
        // 0o700 on purpose: no ordinary umask produces it, so a mode that
        // survives could not have been an accident of the environment.
        let dir = TempDir::new().unwrap();
        let existing = dir.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o700)).unwrap();

        create_all(&existing.join("made")).unwrap();

        assert_eq!(
            mode_of(&existing),
            0o700,
            "a pre-existing level was rewritten"
        );
        assert_eq!(mode_of(&existing.join("made")), 0o755);
    }

    #[test]
    fn a_freshly_created_directory_is_0o755() {
        let dir = TempDir::new().unwrap();
        let made = dir.path().join("made");

        create(&made).unwrap();

        assert_eq!(mode_of(&made), 0o755);
    }

    #[test]
    fn creating_one_that_is_already_there_is_an_error() {
        // The distinction from `create_all`, and staging's reason for wanting
        // it: a staging name that collides with another process's would put
        // two half-built trees in one directory, and the rename would publish
        // whichever lost. Better to fail than to share.
        let dir = TempDir::new().unwrap();

        assert!(create(dir.path()).is_err());
    }

    #[test]
    fn creating_a_directory_that_is_already_there_is_not_an_error() {
        // `populate_virtual_store` and the cache both call this on a path a
        // concurrent install may have just made, and reaching for a directory
        // that has arrived is success, not a race to report.
        let dir = TempDir::new().unwrap();

        create_all(dir.path()).unwrap();
        create_all(dir.path()).unwrap();
    }
}
