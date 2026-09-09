//! Test helpers shared by unit tests and the integration tests in `tests/`.
//!
//! Compiled unconditionally rather than behind `#[cfg(test)]`: integration
//! tests cannot see a parent crate's test-only items, and the alternatives
//! (a `test-support` feature with a self-referential dev-dependency, or
//! duplicating this builder) are worse for a few dozen bytes of binary.

use std::io::Write as _;

use flate2::Compression;
use flate2::write::GzEncoder;

/// sha512 of the three bytes `abc`, base64-encoded, as npm reports it in
/// `dist.integrity`. Verified independently against `sha512sum` rather than
/// produced by the code it is used to test.
pub const ABC_SHA512_SSRI: &str = "sha512-3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU1EI2Q86A4qmslPpUyknw==";

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

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::integrity::Integrity;
use crate::registry::{Dist, Packument, RegistryClient, RegistryError, VersionMetadata};

/// An in-memory registry for tests.
///
/// Counts its calls so tests can prove the store-hit path was taken rather
/// than a redundant download that happened to produce the same result.
#[derive(Default)]
pub struct FixtureRegistry {
    versions: HashMap<(String, String), VersionMetadata>,
    packuments: BTreeMap<String, Packument>,
    tarballs: HashMap<String, Vec<u8>>,
    metadata_calls: AtomicUsize,
    tarball_calls: AtomicUsize,
    packument_calls: AtomicUsize,
    packument_calls_by_name: Mutex<BTreeMap<String, usize>>,
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
            dependencies: BTreeMap::new(),
        };

        self.tarballs.insert(url, tarball);
        self.register(name, version, metadata);
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
            dependencies: BTreeMap::new(),
        };

        self.tarballs.insert(url, tarball);
        self.register(name, version, metadata);
        self
    }

    /// Record one version in both the per-version map and the packument.
    ///
    /// `latest` is the *highest* version registered, not the most recent call,
    /// so registering versions out of order still behaves like a registry.
    fn register(&mut self, name: &str, version: &str, metadata: VersionMetadata) {
        self.versions
            .insert((name.to_string(), version.to_string()), metadata.clone());

        let packument = self
            .packuments
            .entry(name.to_string())
            .or_insert_with(|| Packument {
                name: name.to_string(),
                versions: BTreeMap::new(),
                dist_tags: BTreeMap::new(),
            });
        packument
            .versions
            .insert(version.to_string(), metadata.clone());

        let highest = packument
            .versions_sorted()
            .last()
            .map(|v| v.as_str().to_string())
            .unwrap_or_else(|| version.to_string());
        packument
            .dist_tags
            .insert("latest".to_string(), highest.clone());

        // Spec 1's single-version path resolves `latest` through this map.
        let latest_metadata = packument
            .versions
            .get(&highest)
            .cloned()
            .unwrap_or(metadata);
        self.versions
            .insert((name.to_string(), "latest".to_string()), latest_metadata);
    }

    pub fn metadata_calls(&self) -> usize {
        self.metadata_calls.load(Ordering::Relaxed)
    }

    pub fn tarball_calls(&self) -> usize {
        self.tarball_calls.load(Ordering::Relaxed)
    }

    pub fn packument_calls(&self) -> usize {
        self.packument_calls.load(Ordering::Relaxed)
    }

    /// How many times one package's version list was fetched. Proves the
    /// packument cache works, which an identical resolved graph cannot.
    pub fn packument_calls_for(&self, name: &str) -> usize {
        self.packument_calls_by_name
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or(0)
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

    fn packument(&self, name: &str) -> Result<Packument, RegistryError> {
        self.packument_calls.fetch_add(1, Ordering::Relaxed);
        *self
            .packument_calls_by_name
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_insert(0) += 1;

        self.packuments
            .get(name)
            .cloned()
            .ok_or_else(|| RegistryError::PackageNotFound(name.to_string()))
    }

    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>, RegistryError> {
        self.tarball_calls.fetch_add(1, Ordering::Relaxed);
        self.tarballs
            .get(url)
            .cloned()
            .ok_or_else(|| RegistryError::PackageNotFound(url.to_string()))
    }
}
