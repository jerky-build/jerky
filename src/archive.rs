use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use thiserror::Error;

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
/// The tarball is untrusted input. Two protections apply:
///
/// 1. The leading `package/` component that npm tarballs carry is stripped,
///    so `dest` receives the package's own root.
/// 2. Any entry whose path escapes `dest` is rejected — parent-directory
///    components, absolute paths, and link entries. This is the tar-slip
///    class of bug, and the one place in spec 1 where a mistake is a security
///    hole rather than a crash.
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

        let relative = strip_prefix_component(&raw, &display)?;
        if relative.as_os_str().is_empty() {
            continue;
        }

        let target = dest.join(&relative);

        // `unpack` does not create parent directories. Real npm tarballs
        // usually carry directory entries, but relying on that means a
        // tarball that merely omits them fails to install.
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ArchiveError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        entry
            .unpack(&target)
            .map_err(|source| ArchiveError::Write {
                path: target,
                source,
            })?;
    }

    Ok(())
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
    use crate::testing::{TarEntry, build_tarball};
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

    #[test]
    fn rejects_garbage_that_is_not_gzip() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            extract(b"this is not a gzip stream", dir.path()),
            Err(ArchiveError::Read(_))
        ));
    }
}
