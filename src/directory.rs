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
//! is the one place that says so — to `mkdir` itself rather than in a `chmod`
//! a moment later, because a directory that is corrected to `0o755` was still
//! world-writable for the length of a syscall, and with the worker pool
//! materialising a plan there is somebody else there to notice.

use std::io;
use std::path::Path;

/// The mode every directory jerky creates lands with: traversable and
/// readable by anyone, writable only by its owner.
const MODE: u32 = 0o755;

/// Create `dir` at `0o755`, failing if it is already there.
///
/// The `create_dir` of this module. Reach for it when the caller needs to know
/// it made the directory rather than found it — a staging name, which two
/// processes must not end up sharing.
pub(crate) fn create(dir: &Path) -> io::Result<()> {
    mkdir(dir)?;
    set_mode(dir)
}

/// Create `dir` and every level of it that did not exist, at `0o755`.
///
/// The `create_dir_all` of this module, and the one most callers reach for.
///
/// Only the levels this actually created are chmodded, and that holds under
/// concurrency rather than only on paper: `missing` is a snapshot, so two
/// workers racing for the same level can both believe they must make it, but
/// only one `mkdir` succeeds and the loser leaves the mode alone. A level that
/// was already there belongs to whoever made it, and rewriting its mode would
/// be this deciding something about a directory it did not create. It is also
/// what keeps the extractor cheap: from its second entry onwards a tarball's
/// parent directories all exist, so the common call is answered by one stat
/// instead of a chmod per ancestor per entry — which on a 93k-file tree is
/// several hundred thousand syscalls that change nothing.
pub(crate) fn create_all(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }

    // `is_dir` rather than `exists`, so a plain file — or a dangling symlink —
    // partway down is counted as missing and the `mkdir` for it reports the
    // collision, which is what `create_dir_all` does with the same case. The
    // empty path terminates a relative path's ancestors and is nobody's
    // directory to make.
    let missing: Vec<&Path> = dir
        .ancestors()
        .take_while(|level| !level.as_os_str().is_empty() && !level.is_dir())
        .collect();

    for level in missing.iter().rev() {
        create_level(level)?;
    }
    Ok(())
}

/// Make one level, treating a level that arrived while we were deciding to
/// make it as made.
///
/// The `AlreadyExists` arm is the race between the `missing` snapshot and the
/// `mkdir`, which several workers materialising a plan into one scope
/// directory run into constantly. It insists on a *directory* having arrived,
/// so a file in the way is still the error it is under `create_dir_all`.
fn create_level(dir: &Path) -> io::Result<()> {
    match mkdir(dir) {
        Ok(()) => set_mode(dir),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// `mkdir(dir, 0o755)` — the mode the directory is *born* with.
///
/// The difference from creating it and chmodding it afterwards is a window,
/// not an end state: `std::fs::create_dir` passes `0o777`, so under a
/// permissive umask the directory exists group- and world-writable until the
/// chmod lands, and a sibling worker that checks for it inside that window
/// finds a directory anyone can write to and starts filling it. The umask
/// still applies here, but it can only take bits away, and `set_mode` puts
/// those back.
fn mkdir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new().mode(MODE).create(dir)
}

fn set_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(MODE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use std::thread;
    use tempfile::TempDir;

    /// Enough threads to lose the race most rounds, without turning the test
    /// into a benchmark of the machine it runs on.
    const THREADS: usize = 32;
    const OBSERVERS: usize = 4;

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
    fn a_plain_file_in_the_way_is_an_error_at_any_level() {
        // Walking the levels by hand means writing the collision check that
        // `create_dir_all` used to supply, and the arm that forgives an
        // `AlreadyExists` is exactly where it can go missing. Forgiving a file
        // would hand the caller success and no directory, and it would find
        // that out several writes later.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();

        assert!(
            create_all(&file).is_err(),
            "a file answered as its own leaf"
        );
        assert!(
            create_all(&file.join("under")).is_err(),
            "a file answered as a parent"
        );
    }

    #[test]
    fn several_threads_creating_one_path_all_succeed_and_agree_on_the_mode() {
        // Materialising a plan fans out over a worker pool, and every
        // `@types/*` entry in a graph wants the same `.jerky/@types`
        // directory — so the concurrent call is the ordinary call here, not
        // the exotic one. The threads race for the same levels, and a thread
        // that loses the race has still been handed the directory it asked
        // for: losing must not be an error, and must not leave a level at
        // anything but 0o755.
        let dir = TempDir::new().unwrap();
        let deepest = dir.path().join("a/b/c/d");
        let start = Barrier::new(THREADS);

        thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    start.wait();
                    create_all(&deepest).unwrap();
                });
            }
        });

        let mut level = dir.path().to_path_buf();
        for name in ["a", "b", "c", "d"] {
            level = level.join(name);
            assert_eq!(mode_of(&level), 0o755, "{} is too open", level.display());
        }
    }

    #[test]
    fn a_level_is_never_observed_wider_than_0o755() {
        // The mode at rest is only half the question. Creating a level at the
        // umask's mode and chmodding it afterwards leaves a window in which it
        // exists group- and world-writable, and a sibling worker that passes
        // its own existence check inside that window starts hard-linking a
        // package into a directory anyone can write to. The window is
        // microseconds wide, so this samples for it rather than reasoning
        // about it: observers spin over every level of the path a creator is
        // walking down, and record any mode carrying a bit outside 0o755.
        //
        // Like every mode assertion here it is load bearing only under
        // `umask 0`, which the suite runs separately. A strict umask makes
        // `mkdir`'s mode narrower than 0o755 rather than wider, and narrower
        // is not the hole.
        const ROUNDS: usize = 1000;

        let dir = TempDir::new().unwrap();
        let round = AtomicUsize::new(0);
        let finished = AtomicBool::new(false);
        // 0 means nothing wider was ever seen; any other value is the mode
        // that was, kept so the failure can say what it caught.
        let widest = AtomicU32::new(0);

        thread::scope(|scope| {
            for _ in 0..OBSERVERS {
                scope.spawn(|| {
                    while !finished.load(Ordering::Relaxed) {
                        let mut level = dir.path().join(round.load(Ordering::Relaxed).to_string());
                        for name in ["a", "b", "c", "d"] {
                            level = level.join(name);
                            // A level the creator has not reached yet is the
                            // usual answer, and says nothing either way.
                            if let Ok(metadata) = std::fs::metadata(&level) {
                                let mode = metadata.permissions().mode() & 0o777;
                                if mode & !MODE != 0 {
                                    widest.store(mode, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                });
            }

            for index in 0..ROUNDS {
                round.store(index, Ordering::Relaxed);
                create_all(&dir.path().join(index.to_string()).join("a/b/c/d")).unwrap();
            }
            finished.store(true, Ordering::Relaxed);
        });

        let seen = widest.load(Ordering::Relaxed);
        assert_eq!(seen, 0, "a level was observed at 0o{seen:o}");
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
