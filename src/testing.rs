//! Test helpers shared by unit tests and the integration tests in `tests/`.
//!
//! Compiled unconditionally rather than behind `#[cfg(test)]`: integration
//! tests cannot see a parent crate's test-only items, and the alternatives
//! (a `test-support` feature with a self-referential dev-dependency, or
//! duplicating this builder) are worse for a few dozen bytes of binary.

use std::io::Write as _;

use flate2::Compression;
use flate2::write::GzEncoder;

/// One entry in a generated tarball.
pub enum TarEntry<'a> {
    /// A regular file at `path` with `contents`.
    File { path: &'a str, contents: &'a [u8] },
    /// A symlink at `path` pointing at `target`. Used to build hostile
    /// fixtures that a committed binary tarball could not safely carry.
    Symlink { path: &'a str, target: &'a str },
}

impl<'a> TarEntry<'a> {
    pub fn file(path: &'a str, contents: &'a str) -> Self {
        TarEntry::File {
            path,
            contents: contents.as_bytes(),
        }
    }
}

/// Write a path into a header's name field directly, bypassing the `tar`
/// crate's validation.
///
/// `Builder::append_data` and `Header::set_path` both refuse paths containing
/// `..`, which is exactly what the tar-slip fixtures need to carry. A real
/// attacker writes the archive by hand and is under no such constraint, so a
/// test that cannot express the hostile input cannot test the defence.
fn set_raw_path(header: &mut tar::Header, path: &str) {
    let bytes = path.as_bytes();
    let name = &mut header.as_old_mut().name;
    assert!(
        bytes.len() <= name.len(),
        "fixture path `{path}` exceeds the 100-byte tar name field"
    );
    name[..bytes.len()].copy_from_slice(bytes);
}

/// Build a gzipped tar archive in memory.
///
/// Paths are written verbatim, so callers include the leading `package/`
/// component that real npm tarballs carry — and can deliberately omit it, or
/// write `../` escapes and absolute paths, to exercise the rejection paths.
pub fn build_tarball(entries: &[TarEntry<'_>]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());

    for entry in entries {
        let mut header = tar::Header::new_gnu();
        match entry {
            TarEntry::File { path, contents } => {
                set_raw_path(&mut header, path);
                header.set_size(contents.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append(&header, *contents)
                    .expect("in-memory tar append cannot fail");
            }
            TarEntry::Symlink { path, target } => {
                set_raw_path(&mut header, path);
                header.set_size(0);
                header.set_mode(0o777);
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_link_name(target).expect("link name is valid");
                header.set_cksum();
                builder
                    .append(&header, std::io::empty())
                    .expect("in-memory tar append cannot fail");
            }
        }
    }

    let tar = builder
        .into_inner()
        .expect("in-memory tar finish cannot fail");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&tar).expect("in-memory gzip cannot fail");
    encoder.finish().expect("in-memory gzip cannot fail")
}

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::integrity::Integrity;
use crate::registry::{Dist, RegistryClient, RegistryError, VersionMetadata};

/// An in-memory registry for tests.
///
/// Counts its calls so tests can prove the store-hit path was taken rather
/// than a redundant download that happened to produce the same result.
#[derive(Default)]
pub struct FixtureRegistry {
    versions: HashMap<(String, String), VersionMetadata>,
    tarballs: HashMap<String, Vec<u8>>,
    metadata_calls: AtomicUsize,
    tarball_calls: AtomicUsize,
}

impl FixtureRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a package version, deriving a self-consistent integrity hash
    /// from the tarball bytes so the happy path verifies by construction.
    /// Also registers it under the `latest` tag.
    pub fn with_package(mut self, name: &str, version: &str, tarball: Vec<u8>) -> Self {
        let url = format!("https://fixture.test/{name}/-/{name}-{version}.tgz");
        let integrity = Integrity {
            algo: crate::integrity::Algo::Sha512,
            digest: <sha2::Sha512 as sha2::Digest>::digest(&tarball).to_vec(),
        };

        let metadata = VersionMetadata {
            name: name.to_string(),
            version: version.to_string(),
            dist: Dist {
                tarball: url.clone(),
                integrity: Some(integrity.to_ssri()),
                shasum: None,
            },
        };

        self.tarballs.insert(url, tarball);
        self.versions
            .insert((name.to_string(), version.to_string()), metadata.clone());
        self.versions
            .insert((name.to_string(), "latest".to_string()), metadata);
        self
    }

    /// Register a package whose advertised integrity does not match its bytes.
    pub fn with_corrupt_package(mut self, name: &str, version: &str, tarball: Vec<u8>) -> Self {
        let url = format!("https://fixture.test/{name}/-/{name}-{version}.tgz");
        let wrong = Integrity {
            algo: crate::integrity::Algo::Sha512,
            digest: <sha2::Sha512 as sha2::Digest>::digest(b"not these bytes").to_vec(),
        };

        let metadata = VersionMetadata {
            name: name.to_string(),
            version: version.to_string(),
            dist: Dist {
                tarball: url.clone(),
                integrity: Some(wrong.to_ssri()),
                shasum: None,
            },
        };

        self.tarballs.insert(url, tarball);
        self.versions
            .insert((name.to_string(), version.to_string()), metadata.clone());
        self.versions
            .insert((name.to_string(), "latest".to_string()), metadata);
        self
    }

    pub fn metadata_calls(&self) -> usize {
        self.metadata_calls.load(Ordering::Relaxed)
    }

    pub fn tarball_calls(&self) -> usize {
        self.tarball_calls.load(Ordering::Relaxed)
    }

    fn knows_package(&self, name: &str) -> bool {
        self.versions.keys().any(|(n, _)| n == name)
    }
}

impl RegistryClient for FixtureRegistry {
    fn version_metadata(
        &self,
        name: &str,
        version: &str,
    ) -> Result<VersionMetadata, RegistryError> {
        self.metadata_calls.fetch_add(1, Ordering::Relaxed);
        match self.versions.get(&(name.to_string(), version.to_string())) {
            Some(metadata) => Ok(metadata.clone()),
            None if self.knows_package(name) => Err(RegistryError::VersionNotFound {
                name: name.to_string(),
                version: version.to_string(),
            }),
            None => Err(RegistryError::PackageNotFound(name.to_string())),
        }
    }

    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>, RegistryError> {
        self.tarball_calls.fetch_add(1, Ordering::Relaxed);
        self.tarballs
            .get(url)
            .cloned()
            .ok_or_else(|| RegistryError::PackageNotFound(url.to_string()))
    }
}
