use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use thiserror::Error;

use crate::directory;

#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error("could not read the package tarball")]
    Read(#[source] std::io::Error),
    #[error("tarball entry `{entry}` is unsafe: {reason}")]
    UnsafePath { entry: String, reason: &'static str },
    #[error("failed to write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Extract an npm package tarball into `dest`.
///
/// The tarball is untrusted input. Three protections apply:
///
/// 1. The leading `package/` component that npm tarballs carry is stripped,
///    so `dest` receives the package's own root.
/// 2. Any entry whose path escapes `dest` is rejected — parent-directory
///    components, absolute paths, and link entries. This is the tar-slip
///    class of bug, and the one place in spec 1 where a mistake is a security
///    hole rather than a crash.
/// 3. The recorded mode is discarded and replaced — see `normalise_mode`. The
///    archive chooses the bytes, not the permissions they land with. A
///    directory the tarball omitted is created through [`crate::directory`],
///    which owns the mode of every directory jerky makes.
///
/// Link entries are rejected outright rather than resolved. Some real
/// packages do contain symlinks, so this is a known limitation to revisit
/// when a package that needs them appears.
pub fn extract(tarball: &[u8], dest: &Path) -> Result<(), ArchiveError> {
    let decoder = GzDecoder::new(tarball);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries().map_err(ArchiveError::Read)? {
        let mut entry = entry.map_err(ArchiveError::Read)?;
        let raw = entry.path().map_err(ArchiveError::Read)?.into_owned();
        let display = raw.display().to_string();

        if entry.header().entry_type().is_symlink() || entry.header().entry_type().is_hard_link() {
            return Err(ArchiveError::UnsafePath {
                entry: display,
                reason: "link entries are not supported",
            });
        }

        // Metadata entries name no file: they carry a long path or a set of
        // extended attributes for the entries around them, and `unpack`
        // creates nothing for them. They have to be skipped rather than
        // unpacked, because the code below assumes an entry left something on
        // disk to set permissions on. `pax_global_header` is the common one —
        // `git archive` writes it unconditionally.
        if is_metadata(entry.header().entry_type()) {
            continue;
        }

        let relative = strip_prefix_component(&raw, &display)?;
        if relative.as_os_str().is_empty() {
            continue;
        }

        let target = dest.join(&relative);

        // `unpack` does not create parent directories. Real npm tarballs
        // usually carry directory entries, but relying on that means a
        // tarball that merely omits them fails to install.
        if let Some(parent) = target.parent() {
            // Through `directory` rather than `create_dir_all`, so a
            // directory the tarball never named lands at 0o755 instead of at
            // whatever the umask allows. Only the levels it actually creates
            // are chmodded, which is both the rule — `dest` belongs to the
            // caller, which chose its mode — and what keeps this cheap: from
            // the second entry onwards the parent is already there, and a
            // tarball of tens of thousands of files would otherwise spend a
            // chmod per ancestor per entry.
            directory::create_all(parent).map_err(|source| ArchiveError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        entry
            .unpack(&target)
            .map_err(|source| ArchiveError::Write {
                path: target.clone(),
                source,
            })?;

        normalise_mode(&target, entry.header().mode().unwrap_or(0))?;
    }

    Ok(())
}

/// Whether an entry describes other entries rather than a file of its own.
///
/// This mirrors the set `tar::Entry::unpack` returns early for. Keeping the
/// two in agreement is the price of doing our own work afterwards; the test
/// above fails if they diverge.
fn is_metadata(entry_type: tar::EntryType) -> bool {
    entry_type.is_pax_global_extensions()
        || entry_type.is_pax_local_extensions()
        || entry_type.is_gnu_longname()
        || entry_type.is_gnu_longlink()
}

/// Replace what the tarball recorded with a mode jerky chose.
///
/// Exactly one bit of `recorded` is consulted — owner-execute, which marks a
/// CLI entry point — and only for a file. A directory is always 0o755, since
/// its execute bit is the right to descend into it rather than to run it.
///
/// Whether `target` *is* a directory is read from the filesystem rather than
/// from the header's type flag, because the two disagree. `unpack` applies an
/// old-BSD compatibility rule that makes a non-ustar entry whose name ends in
/// `/` a directory while its flag still says `Regular`; trusting the flag put
/// such a directory at 0o644, which cannot be entered or written into. The
/// same read covers the reverse case, an unrecognised flag that POSIX requires
/// be treated as a regular file. What is on disk is the thing being chmodded,
/// so it is the thing to ask.
///
/// A mode that cannot be read at all is treated as non-executable. That is the
/// conservative direction: the cost is a binary that needs `chmod +x`, against
/// a corrupt header being allowed to choose.
///
fn normalise_mode(target: &Path, recorded: u32) -> Result<(), ArchiveError> {
    use std::os::unix::fs::PermissionsExt as _;

    // Links are rejected before this point, so `symlink_metadata` and
    // `metadata` agree here; the former is used anyway so that a future
    // decision to accept links cannot quietly make this follow one.
    let is_dir = std::fs::symlink_metadata(target)
        .map_err(|source| ArchiveError::Write {
            path: target.to_path_buf(),
            source,
        })?
        .is_dir();

    let mode = if is_dir || recorded & 0o100 != 0 {
        0o755
    } else {
        0o644
    };

    std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode)).map_err(|source| {
        ArchiveError::Write {
            path: target.to_path_buf(),
            source,
        }
    })
}

/// Drop the first path component and reject anything that could escape.
fn strip_prefix_component(raw: &Path, display: &str) -> Result<PathBuf, ArchiveError> {
    let mut components = raw.components().peekable();

    // A leading `./` is noise: GNU tar emits it when archiving a directory.
    // Skipping it before the prefix check keeps ordinary tarballs installable
    // without weakening the check, since `CurDir` cannot move up a level.
    while matches!(components.peek(), Some(Component::CurDir)) {
        components.next();
    }

    // The first component must be a plain directory name — the `package/`
    // prefix. Anything else is already an escape attempt.
    match components.next() {
        Some(Component::Normal(_)) | None => {}
        Some(_) => {
            return Err(ArchiveError::UnsafePath {
                entry: display.to_string(),
                reason: "path is absolute or starts outside the package root",
            });
        }
    }

    let mut relative = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            _ => {
                return Err(ArchiveError::UnsafePath {
                    entry: display.to_string(),
                    reason: "path escapes the destination directory",
                });
            }
        }
    }

    Ok(relative)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TarEntry, build_tarball, mode_of};
    use tempfile::TempDir;

    #[test]
    fn strips_the_leading_package_component() {
        let tarball = build_tarball(&[
            TarEntry::file("package/package.json", r#"{"name":"demo"}"#),
            TarEntry::file("package/lib/index.js", "module.exports = 1;"),
        ]);
        let dir = TempDir::new().unwrap();

        extract(&tarball, dir.path()).unwrap();

        assert!(dir.path().join("package.json").is_file());
        assert!(dir.path().join("lib/index.js").is_file());
        assert!(
            !dir.path().join("package").exists(),
            "prefix must be stripped"
        );
    }

    #[test]
    fn preserves_file_contents() {
        let tarball = build_tarball(&[TarEntry::file("package/index.js", "hello")]);
        let dir = TempDir::new().unwrap();

        extract(&tarball, dir.path()).unwrap();

        let contents = std::fs::read_to_string(dir.path().join("index.js")).unwrap();
        assert_eq!(contents, "hello");
    }

    #[test]
    fn accepts_a_leading_current_directory_component() {
        // GNU tar writes `./package/...` when archiving a directory, so a
        // leading `./` is ordinary output from a common packer, not an escape.
        let tarball = build_tarball(&[TarEntry::file("./package/index.js", "hello")]);
        let dir = TempDir::new().unwrap();

        extract(&tarball, dir.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("index.js")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn rejects_parent_directory_escapes() {
        let tarball = build_tarball(&[TarEntry::file("package/../../evil.js", "pwned")]);
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir(&dest).unwrap();

        assert!(matches!(
            extract(&tarball, &dest),
            Err(ArchiveError::UnsafePath { .. })
        ));
        assert!(
            !dir.path().join("evil.js").exists(),
            "nothing may escape dest"
        );
    }

    #[test]
    fn rejects_absolute_paths() {
        let tarball = build_tarball(&[TarEntry::file("/etc/evil.js", "pwned")]);
        let dir = TempDir::new().unwrap();

        assert!(matches!(
            extract(&tarball, dir.path()),
            Err(ArchiveError::UnsafePath { .. })
        ));
    }

    #[test]
    fn rejects_symlink_entries() {
        let tarball = build_tarball(&[TarEntry::Symlink {
            path: "package/link",
            target: "/etc/passwd",
        }]);
        let dir = TempDir::new().unwrap();

        assert!(matches!(
            extract(&tarball, dir.path()),
            Err(ArchiveError::UnsafePath { .. })
        ));
    }

    #[test]
    fn rejects_an_entry_that_is_only_the_prefix() {
        // A bare `package` entry has nothing left after stripping.
        let tarball = build_tarball(&[TarEntry::file("package", "")]);
        let dir = TempDir::new().unwrap();

        // Stripping yields an empty path, which is skipped rather than an error.
        extract(&tarball, dir.path()).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn a_world_writable_file_is_narrowed_to_its_owner() {
        // The store is machine-global and populated with hard links, so one
        // set of permissions is shared by every project that installs the
        // package. A tarball that records 0o666 would otherwise leave a file
        // in `~/.jerky/store` that any user on the machine can rewrite, and
        // rewriting it changes what every project sees.
        let tarball = build_tarball(&[TarEntry::file_with_mode(
            "package/index.js",
            "module.exports = 1;",
            0o666,
        )]);
        let dir = TempDir::new().unwrap();

        extract(&tarball, dir.path()).unwrap();

        assert_eq!(mode_of(&dir.path().join("index.js")), 0o644);
    }

    #[test]
    fn an_executable_file_stays_executable() {
        // The one bit of the recorded mode that carries meaning: it marks a
        // CLI entry point. #20 links these into `node_modules/.bin`, and a
        // shim pointing at a non-executable file is a runtime failure rather
        // than an install one.
        let tarball = build_tarball(&[TarEntry::file_with_mode(
            "package/bin/cli.js",
            "#!/usr/bin/env node\n",
            0o755,
        )]);
        let dir = TempDir::new().unwrap();

        extract(&tarball, dir.path()).unwrap();

        assert_eq!(mode_of(&dir.path().join("bin/cli.js")), 0o755);
    }

    #[test]
    fn setuid_and_setgid_do_not_survive_extraction() {
        // Already true before this module normalised anything, because
        // `tar::Entry::unpack` masks the recorded mode with `& 0o777` unless
        // asked to preserve it. That is the tar crate's default rather than
        // jerky's invariant, so a crate bump or a stray
        // `set_preserve_permissions(true)` would reopen it silently. This test
        // makes it jerky's property and fails if it stops being one.
        let tarball = build_tarball(&[
            TarEntry::file_with_mode("package/setuid", "", 0o4755),
            TarEntry::file_with_mode("package/setgid", "", 0o2644),
        ]);
        let dir = TempDir::new().unwrap();

        extract(&tarball, dir.path()).unwrap();

        assert_eq!(mode_of(&dir.path().join("setuid")), 0o755);
        assert_eq!(mode_of(&dir.path().join("setgid")), 0o644);
    }

    #[test]
    fn a_directory_is_always_traversable() {
        // A directory's execute bit is the right to descend into it, not the
        // right to run it, so the rule that reads owner-execute off a file
        // must not be applied to one.
        //
        // Not a bug that existed before this module normalised anything —
        // `unpack` did not apply a recorded directory mode either — but the
        // rule introduced here could reintroduce it, and this is what fails
        // if it does.
        let tarball = build_tarball(&[
            TarEntry::dir_with_mode("package/lib", 0o666),
            TarEntry::file("package/lib/index.js", "module.exports = 1;"),
        ]);
        let dir = TempDir::new().unwrap();

        extract(&tarball, dir.path()).unwrap();

        assert_eq!(mode_of(&dir.path().join("lib")), 0o755);
        assert!(dir.path().join("lib/index.js").is_file());
    }

    #[test]
    fn a_global_pax_header_is_not_a_file() {
        // `pax_global_header` is ordinary output — `git archive` emits one on
        // every archive — and it names no file: `unpack` deliberately creates
        // nothing for it. Anything downstream that assumes each entry left
        // something on disk fails here, on a tarball that is not hostile at
        // all, and takes the whole install with it.
        let tarball = build_tarball(&[
            TarEntry::metadata("package/pax_global_header", tar::EntryType::XGlobalHeader),
            TarEntry::file("package/index.js", "module.exports = 1;"),
        ]);
        let dir = TempDir::new().unwrap();

        extract(&tarball, dir.path()).unwrap();

        assert!(dir.path().join("index.js").is_file());
        assert!(
            !dir.path().join("pax_global_header").exists(),
            "a metadata entry must not become a file"
        );
    }

    #[test]
    fn a_directory_the_tarball_omitted_is_still_normalised() {
        // A tarball need not carry directory entries — `extract` tolerates
        // that deliberately — in which case extraction makes them on the way
        // to the file inside, and `create_dir_all` would take their mode from
        // the process umask rather than from any rule of ours. Under a
        // permissive umask that is 0o777, and a world-writable directory in
        // the machine-global store lets another user add or replace files
        // inside a package every project on the machine imports. The file rule
        // would be intact and the hole open anyway.
        //
        // Under a strict umask this holds without the rule, so the `umask 0`
        // run of the suite is what makes it bite — the same bargain every
        // directory-mode assertion in this repo takes.
        let dir = TempDir::new().unwrap();

        let tarball = build_tarball(&[TarEntry::file(
            "package/lib/deep/index.js",
            "module.exports = 1;",
        )]);

        extract(&tarball, dir.path()).unwrap();

        assert_eq!(mode_of(&dir.path().join("lib")), 0o755);
        assert_eq!(mode_of(&dir.path().join("lib/deep")), 0o755);
    }

    #[test]
    fn a_directory_extraction_did_not_create_is_left_alone() {
        // `dest` and anything already inside it belong to the caller. In the
        // real path that is a staging directory the store made at a mode
        // `staging` chose and will rename into place, and extraction has no
        // business overruling it.
        //
        // This is also the hot path: extraction used to re-assert 0o755 on
        // every ancestor of every entry, which on the 93k-file fixture is
        // several hundred thousand chmods that change nothing. Setting the
        // mode only on what it created is what removes them, so a test that
        // one it found keeps its own mode is the test that keeps them gone.
        //
        // 0o700 on purpose: no ordinary umask produces it, so a mode that
        // survives could not be an accident of the environment.
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        std::fs::create_dir(dest.join("lib")).unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(dest.join("lib"), std::fs::Permissions::from_mode(0o700)).unwrap();

        let tarball = build_tarball(&[TarEntry::file(
            "package/lib/deep/index.js",
            "module.exports = 1;",
        )]);

        extract(&tarball, &dest).unwrap();

        assert_eq!(mode_of(&dest), 0o700, "dest belongs to the caller");
        assert_eq!(mode_of(&dest.join("lib")), 0o700, "lib was already there");
        assert_eq!(mode_of(&dest.join("lib/deep")), 0o755, "deep is ours");
    }

    #[test]
    fn a_directory_is_recognised_by_the_filesystem_not_the_type_flag() {
        // `tar` honours an old BSD rule: a non-ustar entry whose name ends in
        // `/` becomes a directory even though its type flag says regular
        // file. A rule that asked the flag would put that directory at 0o644,
        // which cannot be entered or written into — and nothing about the
        // tarball is hostile, it is merely old.
        let tarball = build_tarball(&[TarEntry::OldStyleDir {
            path: "package/lib/",
            mode: 0o644,
        }]);
        let dir = TempDir::new().unwrap();

        extract(&tarball, dir.path()).unwrap();

        let lib = dir.path().join("lib");
        assert!(lib.is_dir(), "the trailing slash makes this a directory");
        assert_eq!(mode_of(&lib), 0o755);
    }

    #[test]
    fn rejects_garbage_that_is_not_gzip() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            extract(b"this is not a gzip stream", dir.path()),
            Err(ArchiveError::Read(_))
        ));
    }
}
