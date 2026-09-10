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

/// One version in a fixture: its number and the dependencies it declares.
pub type FixtureVersion<'a> = (&'a str, &'a [(&'a str, &'a str)]);

/// One entry in a fixture tree: a package name, a version, and its
/// dependencies as `(name, range)` pairs.
pub type FixtureEntry<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

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
    packument_calls: Mutex<BTreeMap<String, usize>>,
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

    /// Register several versions of one package, each with its own
    /// dependencies, as a registry would report them in one packument.
    ///
    /// Each entry is `(version, &[(dep_name, dep_range)])`. Tarballs are
    /// generated and hashed so every version verifies by construction, and
    /// `latest` resolves to the highest version given.
    ///
    /// This is what lets a test express a dependency *edge*: `with_package`
    /// alone can only register leaves.
    pub fn with_packument(mut self, name: &str, versions: &[FixtureVersion<'_>]) -> Self {
        for (version, dependencies) in versions {
            let tarball = build_tarball(&[TarEntry::file(
                "package/package.json",
                &format!(r#"{{"name":"{name}","version":"{version}"}}"#),
            )]);
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
                dependencies: dependencies
                    .iter()
                    .map(|(n, r)| (n.to_string(), r.to_string()))
                    .collect(),
            };

            self.tarballs.insert(url, tarball);
            self.register(name, version, metadata);
        }
        self
    }

    /// Register a whole dependency tree as a flat list of
    /// `(name, version, &[(dep_name, dep_range)])`.
    ///
    /// Entries for the same name are collected into one packument, so a tree
    /// can list several versions of a package the way a real registry holds
    /// them. This reads as a tree at the call site, which is what the
    /// resolver's tests are about:
    ///
    /// ```ignore
    /// FixtureRegistry::new().with_tree(&[
    ///     ("a", "1.0.0", &[("b", "^1.0.0")]),
    ///     ("b", "1.0.0", &[]),
    /// ]);
    /// ```
    pub fn with_tree(mut self, entries: &[FixtureEntry<'_>]) -> Self {
        for (name, version, dependencies) in entries {
            self = self.with_packument(name, &[(version, dependencies)]);
        }
        self
    }

    /// Register a package whose advertised integrity does not match its bytes.
    ///
    /// Note that `latest` resolves to the *highest* version registered, so if
    /// a higher intact version of the same package is also registered, `latest`
    /// will not reach this one. Request it by exact version in that case.
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
        // `highest` is always a key of `versions` — it came from sorting them —
        // so this lookup cannot miss.
        let latest_metadata = packument
            .versions
            .get(&highest)
            .cloned()
            .expect("highest came from these versions");
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
        self.packument_calls.lock().unwrap().values().sum()
    }

    /// How many times one package's version list was fetched. Proves the
    /// packument cache works, which an identical resolved graph cannot.
    pub fn packument_calls_for(&self, name: &str) -> usize {
        self.packument_calls
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
        *self
            .packument_calls
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::RegistryClient;

    #[test]
    fn with_packument_registers_versions_and_their_edges() {
        let registry = FixtureRegistry::new().with_packument(
            "a",
            &[("1.0.0", &[("b", "^1.0.0")]), ("1.2.0", &[("b", "^2.0.0")])],
        );

        let p = registry.packument("a").unwrap();
        assert_eq!(p.versions.len(), 2);
        assert_eq!(
            p.resolve_tag("latest"),
            Some("1.2.0"),
            "latest is the highest"
        );
        assert_eq!(p.versions["1.0.0"].dependencies["b"], "^1.0.0");
        assert_eq!(p.versions["1.2.0"].dependencies["b"], "^2.0.0");
    }

    #[test]
    fn fixture_versions_verify_against_their_own_tarballs() {
        // The point of deriving integrity from the generated bytes: the happy
        // path must verify by construction, or every install test is testing
        // the failure path by accident.
        let registry = FixtureRegistry::new().with_packument("a", &[("1.0.0", &[])]);

        let p = registry.packument("a").unwrap();
        let metadata = &p.versions["1.0.0"];
        let bytes = registry.fetch_tarball(&metadata.dist.tarball).unwrap();

        assert!(metadata.dist.integrity().unwrap().verify(&bytes).is_ok());
    }

    #[test]
    fn latest_is_the_highest_version_not_the_last_registered() {
        // Registered out of order on purpose: a registry does not care which
        // order a test happened to declare things in.
        let registry = FixtureRegistry::new()
            .with_packument("a", &[("2.0.0", &[])])
            .with_packument("a", &[("1.0.0", &[])]);

        assert_eq!(
            registry.packument("a").unwrap().resolve_tag("latest"),
            Some("2.0.0")
        );
    }

    #[test]
    fn packument_calls_are_counted_per_package() {
        let registry = FixtureRegistry::new()
            .with_packument("a", &[("1.0.0", &[])])
            .with_packument("b", &[("1.0.0", &[])]);

        registry.packument("a").unwrap();
        registry.packument("a").unwrap();
        registry.packument("b").unwrap();

        assert_eq!(registry.packument_calls_for("a"), 2);
        assert_eq!(registry.packument_calls_for("b"), 1);
        assert_eq!(registry.packument_calls(), 3, "the total is the sum");
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;
    use crate::registry::RegistryClient;

    #[test]
    fn with_tree_collects_repeated_names_into_one_packument() {
        // Two versions of `d` listed separately must not clobber each other:
        // a registry holds both, and the conflict test depends on it.
        let registry = FixtureRegistry::new().with_tree(&[
            ("d", "1.5.0", &[]),
            ("d", "2.1.0", &[]),
            ("a", "1.0.0", &[("d", "^1.0.0")]),
        ]);

        let d = registry.packument("d").unwrap();
        assert_eq!(d.versions.len(), 2);
        assert_eq!(d.resolve_tag("latest"), Some("2.1.0"));

        let a = registry.packument("a").unwrap();
        assert_eq!(a.versions["1.0.0"].dependencies["d"], "^1.0.0");
    }
}
