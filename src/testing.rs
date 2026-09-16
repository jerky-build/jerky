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
    /// A regular file at `path` with `contents`, recorded with `mode`.
    File {
        path: &'a str,
        contents: &'a [u8],
        mode: u32,
    },
    /// A directory at `path`, recorded with `mode`. Real npm tarballs carry
    /// directory entries, and their mode is as untrusted as a file's.
    Dir { path: &'a str, mode: u32 },
    /// A symlink at `path` pointing at `target`. Used to build hostile
    /// fixtures that a committed binary tarball could not safely carry.
    Symlink { path: &'a str, target: &'a str },
    /// A directory as a pre-ustar packer wrote one: the type flag says
    /// regular file and the trailing `/` in the name is what marks it a
    /// directory. `tar` honours that old BSD rule, so an entry of this shape
    /// becomes a directory on disk while its flag still says otherwise — and
    /// anything that reads the flag rather than the filesystem gets it wrong.
    OldStyleDir { path: &'a str, mode: u32 },
    /// An entry carrying a type flag of the fixture's choosing.
    ///
    /// For the metadata entries real packers emit — a `pax_global_header`
    /// above all — which name no file and unpack to nothing. A fixture builder
    /// that could only express entries which become files could not catch the
    /// code that assumes every entry does.
    Metadata {
        path: &'a str,
        entry_type: tar::EntryType,
    },
}

impl<'a> TarEntry<'a> {
    pub fn file(path: &'a str, contents: &'a str) -> Self {
        TarEntry::File {
            path,
            contents: contents.as_bytes(),
            mode: 0o644,
        }
    }

    /// A directory carrying a mode of the fixture's choosing.
    pub fn dir_with_mode(path: &'a str, mode: u32) -> Self {
        TarEntry::Dir { path, mode }
    }

    /// An entry that names no file of its own — a `pax_global_header`, a GNU
    /// longname — which unpacks to nothing.
    pub fn metadata(path: &'a str, entry_type: tar::EntryType) -> Self {
        TarEntry::Metadata { path, entry_type }
    }

    /// A file carrying a mode of the fixture's choosing.
    ///
    /// A package tarball is untrusted input and the mode field is part of it,
    /// so the hostile modes — setuid, setgid, world-writable — have to be
    /// expressible here or the normalisation in `archive::extract` cannot be
    /// tested at all.
    pub fn file_with_mode(path: &'a str, contents: &'a str, mode: u32) -> Self {
        TarEntry::File {
            path,
            contents: contents.as_bytes(),
            mode,
        }
    }
}

/// The mode a path carries on disk, as the low twelve bits.
///
/// Shared rather than written twice: the extraction unit tests and the
/// install tests both assert on modes, and two copies of the masking would be
/// two chances to mask differently.
pub fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
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
            TarEntry::File {
                path,
                contents,
                mode,
            } => {
                set_raw_path(&mut header, path);
                header.set_size(contents.len() as u64);
                header.set_mode(*mode);
                header.set_cksum();
                builder
                    .append(&header, *contents)
                    .expect("in-memory tar append cannot fail");
            }
            TarEntry::Dir { path, mode } => {
                set_raw_path(&mut header, path);
                header.set_size(0);
                header.set_mode(*mode);
                header.set_entry_type(tar::EntryType::Directory);
                header.set_cksum();
                builder
                    .append(&header, std::io::empty())
                    .expect("in-memory tar append cannot fail");
            }
            TarEntry::OldStyleDir { path, mode } => {
                // `Header::new_old()` rather than `new_gnu()`: tar applies the
                // trailing-slash rule only when the header is not ustar.
                let mut old = tar::Header::new_old();
                set_raw_path(&mut old, path);
                old.set_size(0);
                old.set_mode(*mode);
                old.set_entry_type(tar::EntryType::Regular);
                old.set_cksum();
                builder
                    .append(&old, std::io::empty())
                    .expect("in-memory tar append cannot fail");
            }
            TarEntry::Metadata { path, entry_type } => {
                set_raw_path(&mut header, path);
                header.set_size(0);
                header.set_mode(0o644);
                header.set_entry_type(*entry_type);
                header.set_cksum();
                builder
                    .append(&header, std::io::empty())
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::binaries::Declared;
use crate::integrity::Integrity;
use crate::registry::{
    Dist, Fetched, Freshness, Packument, PeerMeta, RegistryClient, RegistryError, VersionMetadata,
};

/// One version in a fixture: its number and the dependencies it declares.
pub type FixtureVersion<'a> = (&'a str, &'a [(&'a str, &'a str)]);

/// One entry in a fixture tree: a package name, a version, and its
/// dependencies as `(name, range)` pairs.
pub type FixtureEntry<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

/// How long a rendezvous waits before deciding the callers it is waiting for
/// are never going to arrive.
///
/// Generous, because it is only ever paid by a *failing* test: a registry
/// fetching in parallel meets the rendezvous in microseconds. A serial one
/// pays this once — the first waiter times out, marks the rendezvous broken,
/// and releases every later caller immediately — rather than once per package,
/// which is what keeps a regression from turning into a suite that hangs.
const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(5);

/// A meeting point that proves callers are genuinely concurrent.
///
/// Asserting on a high-water mark of in-flight calls can pass by luck: a
/// serial implementation that happens to be preempted never overlaps, but a
/// parallel one under a loaded CI box might only ever reach two. This instead
/// makes each caller *wait* for its peers, so the only way through is for
/// `width` calls to be in flight at once. A serial implementation cannot
/// satisfy it at any speed, which is the point.
#[derive(Debug)]
struct Rendezvous {
    width: usize,
    /// The names that meet here, or `None` for every caller.
    ///
    /// A gate every caller meets can only ever prove that callers *on one
    /// level* overlapped, because the gate is synchronous: nothing it holds
    /// completes, so nothing below it is ever discovered. Naming the
    /// participants is what lets a test hold one fetch open while the walk
    /// descends past it on another branch, which is the only way to put two
    /// different depths in flight at once.
    only: Option<BTreeSet<String>>,
    state: Mutex<RendezvousState>,
    arrived: Condvar,
}

#[derive(Debug, Default)]
struct RendezvousState {
    in_flight: usize,
    high_water: usize,
    /// Set once `width` callers have been in flight together, and never
    /// cleared.
    ///
    /// The predicate waiters block on has to be monotonic. Keying it on
    /// `in_flight` instead is a lost-wakeup race: the caller that satisfies the
    /// rendezvous goes on to decrement `in_flight` while still holding the
    /// lock, so by the time a waiter re-checks, the count has already fallen
    /// back below `width` and it waits again — forever, or until the timeout
    /// calls a working implementation broken.
    released: bool,
    /// Set when a waiter gives up, and never cleared. It is what a test reads
    /// to distinguish "ran in parallel" from "timed out and carried on".
    broken: bool,
}

impl Rendezvous {
    fn new(width: usize) -> Self {
        Self {
            width,
            only: None,
            state: Mutex::new(RendezvousState::default()),
            arrived: Condvar::new(),
        }
    }

    /// A rendezvous only these names meet. Everything else passes straight
    /// through without waiting and without being counted.
    fn only(names: &[&str], width: usize) -> Self {
        Self {
            only: Some(names.iter().map(|name| (*name).to_string()).collect()),
            ..Self::new(width)
        }
    }

    /// Does `name` meet this gate at all?
    fn admits(&self, name: &str) -> bool {
        match &self.only {
            None => true,
            Some(only) => only.contains(name),
        }
    }

    /// Block until `width` callers are inside, then let them all through.
    ///
    /// Callers arriving after the rendezvous has been satisfied pass straight
    /// through. The claim being tested is that `width` calls were in flight at
    /// one moment, which is settled the first time it opens.
    fn meet(&self) {
        let mut state = self.state.lock().unwrap();
        state.in_flight += 1;
        state.high_water = state.high_water.max(state.in_flight);

        if state.in_flight >= self.width {
            state.released = true;
            self.arrived.notify_all();
        } else if !state.released {
            // `wait_timeout_while` rather than a bare wait: a serial caller
            // would otherwise block forever and take the test suite with it.
            let (guard, timed_out) = self
                .arrived
                .wait_timeout_while(state, RENDEZVOUS_TIMEOUT, |s| !s.released)
                .unwrap();
            state = guard;
            if timed_out.timed_out() {
                state.broken = true;
                // Releases everyone else too, so a serial implementation pays
                // one timeout for the whole run rather than one per package.
                state.released = true;
                self.arrived.notify_all();
            }
        }

        state.in_flight -= 1;
    }
}

/// An in-memory registry for tests.
///
/// Counts its calls so tests can prove the store-hit path was taken rather
/// than a redundant download that happened to produce the same result.
#[derive(Default)]
pub struct FixtureRegistry {
    versions: HashMap<(String, String), VersionMetadata>,
    /// What every request sees, unless the name also has a current view.
    packuments: BTreeMap<String, Packument>,
    /// What a `MustBeCurrent` request sees, for the names that have one.
    ///
    /// Empty for every fixture that has not asked for one, which is why the
    /// rest of the suite is unaffected by its existence.
    current: BTreeMap<String, Packument>,
    tarballs: HashMap<String, Vec<u8>>,
    metadata_calls: AtomicUsize,
    tarball_calls: AtomicUsize,
    tarball_calls_by_name: Mutex<BTreeMap<String, usize>>,
    packument_calls: Mutex<BTreeMap<String, usize>>,
    /// `None` until a test asks for one, so every existing fixture is
    /// unaffected.
    tarball_rendezvous: Option<Rendezvous>,
    packument_rendezvous: Option<Rendezvous>,
    packument_handshake: Option<Handshake>,
    tarballs_in_flight: Mutex<InFlight>,
    packuments_in_flight: Mutex<InFlight>,
}

/// A one-way gate between two *different* kinds of call.
///
/// [`Rendezvous`] proves that several calls of one kind overlapped. This
/// proves something a rendezvous structurally cannot: that a call of one kind
/// was in flight while a call of another kind had not yet returned — an
/// ordering *between* phases rather than a width *within* one. It is what a
/// test asks when the claim is "the second phase had started before the first
/// one finished", which is the whole of what an overlapped fetch is.
///
/// One side waits and the other opens, and they never swap roles, so there is
/// no width to satisfy and no possibility of the two sides deadlocking against
/// each other. The timeout is the same one a rendezvous uses and is there for
/// the same reason: an implementation that never opens the gate must fail the
/// test rather than hang the suite.
#[derive(Debug)]
struct Handshake {
    /// The names that wait here. Everything else passes straight through.
    only: BTreeSet<String>,
    state: Mutex<HandshakeState>,
    opened: Condvar,
}

#[derive(Debug, Default)]
struct HandshakeState {
    /// Set by the opening side, and never cleared — the claim is that the two
    /// were in flight together once, which the first opening settles.
    open: bool,
    /// Set when a waiter gives up. What a test reads to tell "the gate was
    /// opened" from "nobody ever opened it and we carried on".
    broken: bool,
}

impl Handshake {
    fn new(names: &[&str]) -> Self {
        Self {
            only: names.iter().map(|name| (*name).to_string()).collect(),
            state: Mutex::new(HandshakeState::default()),
            opened: Condvar::new(),
        }
    }

    /// Let everyone through, now and from now on.
    fn open(&self) {
        let mut state = self.state.lock().unwrap();
        state.open = true;
        self.opened.notify_all();
    }

    /// Block until the gate is opened, if this name waits here at all.
    fn wait(&self, name: &str) {
        if !self.only.contains(name) {
            return;
        }

        let mut state = self.state.lock().unwrap();
        if state.open {
            return;
        }

        let (guard, timed_out) = self
            .opened
            .wait_timeout_while(state, RENDEZVOUS_TIMEOUT, |s| !s.open)
            .unwrap();
        state = guard;
        if timed_out.timed_out() {
            state.broken = true;
            // Releases every other waiter too, so an implementation that
            // never opens the gate pays one timeout for the run rather than
            // one per held name.
            state.open = true;
            self.opened.notify_all();
        }
    }

    fn held(&self) -> bool {
        !self.state.lock().unwrap().broken
    }
}

/// Calls currently inside one registry method, and the most there have ever
/// been.
///
/// Separate from [`Rendezvous`], which also counts arrivals but only across
/// the window a caller spends *at the gate*: a caller that has passed through
/// and is building its response is no longer counted there while still being
/// in flight. This wraps the whole call instead, so its peak is exact, and a
/// peak above a cap is a real failure rather than a coincidence that happened
/// to be observed.
#[derive(Debug, Default)]
struct InFlight {
    current: usize,
    peak: usize,
}

impl InFlight {
    fn enter(counter: &Mutex<Self>) {
        let mut state = counter.lock().unwrap();
        state.current += 1;
        state.peak = state.peak.max(state.current);
    }

    fn leave(counter: &Mutex<Self>) {
        counter.lock().unwrap().current -= 1;
    }
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
            optional_dependencies: BTreeMap::new(),
            os: Vec::new(),
            cpu: Vec::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            bin: None,
        };

        self.tarballs.insert(url, tarball);
        self.register(name, version, metadata);
        self
    }

    /// Declare peers on a version that is already registered.
    ///
    /// Separate from the builders that register a version, rather than a
    /// parameter on each of them: peers appear in a handful of tests and
    /// nowhere else, and threading an almost-always-empty argument through
    /// every call site would cost every existing fixture a `&[]`.
    ///
    /// Each entry is `(name, range, optional)`.
    pub fn with_declared_peers(
        self,
        name: &str,
        version: &str,
        peers: &[(&str, &str, bool)],
    ) -> Self {
        let amend = |metadata: &mut VersionMetadata| {
            for (peer, range, optional) in peers {
                metadata
                    .peer_dependencies
                    .insert(peer.to_string(), range.to_string());
                if *optional {
                    metadata
                        .peer_dependencies_meta
                        .insert(peer.to_string(), PeerMeta { optional: true });
                }
            }
        };

        self.amend(name, version, amend)
    }

    /// Declare `optionalDependencies` on a version that is already registered.
    ///
    /// Separate from the builders that register a version, for the reason
    /// [`FixtureRegistry::with_declared_peers`] is: optional dependencies
    /// appear in a handful of tests, and threading an almost-always-empty
    /// argument through every call site would cost every existing fixture a
    /// `&[]`.
    ///
    /// Each entry is `(name, range)`.
    pub fn with_optional_dependencies(
        self,
        name: &str,
        version: &str,
        optional: &[(&str, &str)],
    ) -> Self {
        self.amend(name, version, |metadata| {
            for (dependency, range) in optional {
                metadata
                    .optional_dependencies
                    .insert(dependency.to_string(), range.to_string());
            }
        })
    }

    /// Declare `os` and `cpu` on a version that is already registered.
    ///
    /// Tests that must be about a machine other than the one running them use
    /// the two constants jerky's own target list provides: `["win32"]` is a
    /// platform this project has an invariant against ever supporting, and
    /// `["linux", "darwin"]` covers every platform it does. Both are therefore
    /// the same answer on every machine the suite can run on.
    pub fn with_platform(self, name: &str, version: &str, os: &[&str], cpu: &[&str]) -> Self {
        self.amend(name, version, |metadata| {
            metadata.os = os.iter().map(|value| value.to_string()).collect();
            metadata.cpu = cpu.iter().map(|value| value.to_string()).collect();
        })
    }

    /// Declare `bin` on a version that is already registered.
    ///
    /// The object form, which is what the registry serves — the string form is
    /// a parsing question rather than an installing one, and is covered where
    /// it is parsed.
    pub fn with_bins(self, name: &str, version: &str, bins: &[(&str, &str)]) -> Self {
        self.amend(name, version, |metadata| {
            metadata.bin = Some(Declared::Many(
                bins.iter()
                    .map(|(bin, target)| (bin.to_string(), target.to_string()))
                    .collect(),
            ));
        })
    }

    /// Apply an edit to one registered version, in both places it is held.
    ///
    /// Both copies, because `register` cloned the metadata into each and a
    /// fixture that amended only one would resolve differently depending on
    /// whether the packument or the version map was consulted.
    ///
    /// Panics on a name or version that is not registered, for the reason
    /// `with_dist_tag` does: silently amending nothing turns a typo into a
    /// fixture without the thing under test, and a green test that proves the
    /// opposite of what it claims.
    fn amend(mut self, name: &str, version: &str, edit: impl Fn(&mut VersionMetadata)) -> Self {
        let metadata = self
            .versions
            .get_mut(&(name.to_string(), version.to_string()))
            .unwrap_or_else(|| panic!("no `{name}` at `{version}` to amend"));
        edit(metadata);

        let packument = self
            .packuments
            .get_mut(name)
            .unwrap_or_else(|| panic!("no packument for `{name}`"));
        let metadata = packument
            .versions
            .get_mut(version)
            .unwrap_or_else(|| panic!("no packument entry for `{name}` at `{version}`"));
        edit(metadata);

        // `register` also files a copy of the highest stable version under the
        // `latest` key, for the single-version path that resolves a dist-tag
        // through the version map. Amending the version without refreshing
        // that copy leaves the two disagreeing, so a fixture amended after
        // registration would behave differently depending on whether the test
        // asked for `1.0.0` or for `latest`.
        if packument.dist_tags.get("latest").map(String::as_str) == Some(version) {
            let refreshed = packument.versions[version].clone();
            self.versions
                .insert((name.to_string(), "latest".to_string()), refreshed);
        }

        self
    }

    /// Point a dist-tag at a version that is already registered.
    ///
    /// `latest` is maintained automatically by every other builder, so this is
    /// for the tags that are not: `next`, `beta`, and anything else a
    /// publisher invents. Panics if the version is unknown, because a dangling
    /// tag is a thing to test deliberately rather than to create by typo.
    pub fn with_dist_tag(mut self, name: &str, tag: &str, version: &str) -> Self {
        let packument = self
            .packuments
            .get_mut(name)
            .unwrap_or_else(|| panic!("no packument for `{name}`"));
        assert!(
            packument.versions.contains_key(version),
            "`{name}` has no version `{version}` to tag `{tag}`"
        );
        packument
            .dist_tags
            .insert(tag.to_string(), version.to_string());
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
            let (metadata, tarball) = Self::build_version(name, version, dependencies);
            self.tarballs.insert(metadata.dist.tarball.clone(), tarball);
            self.register(name, version, metadata);
        }
        self
    }

    /// One version's metadata and the tarball its integrity hash is taken
    /// from, self-consistent by construction.
    ///
    /// Shared rather than written twice: the current-view builder below needs
    /// exactly the same version, and two spellings of how a fixture version is
    /// made would be two chances for a hash to stop matching its bytes.
    fn build_version(
        name: &str,
        version: &str,
        dependencies: &[(&str, &str)],
    ) -> (VersionMetadata, Vec<u8>) {
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
                tarball: url,
                integrity: Some(integrity.to_ssri()),
                shasum: None,
            },
            dependencies: dependencies
                .iter()
                .map(|(n, r)| (n.to_string(), r.to_string()))
                .collect(),
            // Optional-free and peer-free, and platform-free with it: a
            // fixture needing any of the three amends the version afterwards.
            optional_dependencies: BTreeMap::new(),
            os: Vec::new(),
            cpu: Vec::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            bin: None,
        };

        (metadata, tarball)
    }

    /// Publish a version that only a `MustBeCurrent` request can see.
    ///
    /// The one thing that makes [`Freshness`] observable at all. Every other
    /// builder here registers a package that answers identically whatever is
    /// asked of it, and the default `packument` drops its `freshness` argument
    /// on the floor -- so without this, no test in the suite can tell the two
    /// questions apart, and a resolver that confused them would pass every one
    /// of them.
    ///
    /// What it models is a release published *inside* the metadata cache's
    /// freshness window. A range may be answered from the window, so it
    /// selects from the versions the cache knew about; a dist-tag must reach
    /// the registry, so it sees this one too. The fixture holds no cache of
    /// its own -- the split between the two views is the fiction that stands
    /// in for one, which is what keeps this usable by a test driving the
    /// resolver directly.
    ///
    /// The current view is a superset of the cached one, because that is what
    /// a publish looks like, and `latest` moves with it.
    pub fn with_current_version(
        mut self,
        name: &str,
        version: &str,
        dependencies: &[(&str, &str)],
    ) -> Self {
        let mut packument = self
            .current
            .get(name)
            .or_else(|| self.packuments.get(name))
            .unwrap_or_else(|| panic!("no packument for `{name}` to publish onto"))
            .clone();

        let (metadata, tarball) = Self::build_version(name, version, dependencies);
        self.tarballs.insert(metadata.dist.tarball.clone(), tarball);
        packument.versions.insert(version.to_string(), metadata);

        // The same rule `register` applies: `latest` is the highest stable
        // version, not the most recent call.
        let sorted = packument.versions_sorted();
        let highest = sorted
            .iter()
            .rev()
            .find(|candidate| !candidate.is_prerelease())
            .or_else(|| sorted.last())
            .map(|v| v.as_str().to_string())
            .unwrap_or_else(|| version.to_string());
        packument.dist_tags.insert("latest".to_string(), highest);

        self.current.insert(name.to_string(), packument);
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
            optional_dependencies: BTreeMap::new(),
            os: Vec::new(),
            cpu: Vec::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            bin: None,
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

        // The highest *stable* version, not simply the highest. A registry
        // does not point `latest` at a prerelease, and a fixture that does
        // makes `latest` reach 5.0.0-beta.1 in a packument holding 4.17.21 —
        // quietly turning any test that registers a prerelease into a test of
        // behaviour npm does not have. Falling back to the highest overall
        // keeps a fixture of nothing but prereleases resolvable, which is the
        // only case where a real registry would do the same.
        let sorted = packument.versions_sorted();
        let highest = sorted
            .iter()
            .rev()
            .find(|candidate| !candidate.is_prerelease())
            .or_else(|| sorted.last())
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

    /// How many times one package's tarball was requested.
    ///
    /// The counterpart to [`Self::packument_calls_for`], and needed for the
    /// same reason: once downloads overlap resolution, a total says only that
    /// *something* was fetched, and the interesting claims are about a
    /// particular package — that the one a gate refuses was never asked for,
    /// or that a warm entry was never asked for twice.
    pub fn tarball_calls_for(&self, name: &str) -> usize {
        self.tarball_calls_by_name
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or(0)
    }

    /// Make every `fetch_tarball` wait until `width` of them are in flight.
    ///
    /// The fixture is what makes parallelism observable at all: the bytes are
    /// already in memory, so a download costs nothing and overlap is invisible
    /// without somewhere to stand still. A serial installer deadlocks against
    /// this until the rendezvous gives up, and `tarballs_met_rendezvous` then
    /// reports false.
    pub fn with_tarball_rendezvous(mut self, width: usize) -> Self {
        self.tarball_rendezvous = Some(Rendezvous::new(width));
        self
    }

    /// Whether the rendezvous was satisfied rather than timed out.
    pub fn tarballs_met_rendezvous(&self) -> bool {
        self.tarball_rendezvous
            .as_ref()
            .is_some_and(|r| !r.state.lock().unwrap().broken)
    }

    /// The most `fetch_tarball` calls ever in flight at once, exactly.
    pub fn peak_concurrent_tarballs(&self) -> usize {
        self.tarballs_in_flight.lock().unwrap().peak
    }

    /// Make every `packument` wait until `width` of them are in flight.
    ///
    /// The resolver's counterpart to [`Self::with_tarball_rendezvous`], and
    /// necessary for the same reason: a fixture answers from memory, so
    /// overlap is invisible unless the calls are given somewhere to stand
    /// still. A resolver that walks one packument at a time cannot satisfy it
    /// at any speed.
    pub fn with_packument_rendezvous(mut self, width: usize) -> Self {
        self.packument_rendezvous = Some(Rendezvous::new(width));
        self
    }

    /// Make the `packument` calls for these names — and only these — wait
    /// until `width` of them are in flight.
    ///
    /// What [`Self::with_packument_rendezvous`] cannot express. That gate
    /// stops every caller, so the only fetches that can ever meet at it are
    /// ones the resolver had already started together; a fetch it is holding
    /// never completes, so nothing that depends on it is ever discovered, and
    /// a level-synchronous walk and a continuous one are indistinguishable.
    /// Naming the participants leaves every other fetch free to complete, so
    /// the walk goes on descending past a held one — and a gate naming a
    /// package at depth 0 and another at depth N is met only if those two
    /// depths were genuinely in flight together.
    pub fn with_packument_rendezvous_for(mut self, names: &[&str], width: usize) -> Self {
        self.packument_rendezvous = Some(Rendezvous::only(names, width));
        self
    }

    /// Hold the `packument` calls for these names until *some* tarball has
    /// been asked for.
    ///
    /// The gate an overlapped fetch has to open. Held names sit at a depth the
    /// crawl can only reach by first resolving something above them, so the
    /// only tarball that can open the gate is one whose version was selected
    /// while resolution was still going — which is precisely the thing a
    /// resolve-then-fetch barrier makes impossible. An installer with that
    /// barrier waits out the timeout and
    /// [`Self::packuments_awaited_a_tarball`] then reports false.
    pub fn with_packument_awaiting_a_tarball(mut self, names: &[&str]) -> Self {
        self.packument_handshake = Some(Handshake::new(names));
        self
    }

    /// Whether the held packuments were released by a tarball rather than by
    /// the timeout.
    pub fn packuments_awaited_a_tarball(&self) -> bool {
        self.packument_handshake
            .as_ref()
            .is_some_and(Handshake::held)
    }

    /// Whether the packument rendezvous was satisfied rather than timed out.
    pub fn packuments_met_rendezvous(&self) -> bool {
        self.packument_rendezvous
            .as_ref()
            .is_some_and(|r| !r.state.lock().unwrap().broken)
    }

    /// The most `packument` calls ever in flight at once, exactly.
    pub fn peak_concurrent_packuments(&self) -> usize {
        self.packuments_in_flight.lock().unwrap().peak
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

    /// Which package a fixture tarball URL belongs to.
    ///
    /// Read back out of the registered metadata rather than parsed out of the
    /// URL, so the two spellings cannot drift apart.
    fn name_of_tarball(&self, url: &str) -> Option<String> {
        self.versions
            .values()
            .find(|metadata| metadata.dist.tarball == url)
            .map(|metadata| metadata.name.clone())
    }

    fn knows_package(&self, name: &str) -> bool {
        self.versions.keys().any(|(n, _)| n == name)
    }

    /// Answer one packument request: count it, let the rendezvous see it, and
    /// hand back the view the requested freshness is entitled to.
    ///
    /// One body for both entry points, so a request is counted once however
    /// it arrived and the two views cannot drift apart.
    fn answer(&self, name: &str, freshness: Freshness) -> Result<Packument, RegistryError> {
        *self
            .packument_calls
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_insert(0) += 1;

        InFlight::enter(&self.packuments_in_flight);
        if let Some(rendezvous) = &self.packument_rendezvous
            && rendezvous.admits(name)
        {
            rendezvous.meet();
        }
        if let Some(handshake) = &self.packument_handshake {
            handshake.wait(name);
        }

        // A name with no current view answers the same thing either way, which
        // is every fixture that has not deliberately asked to be otherwise.
        let view = match freshness {
            Freshness::MustBeCurrent => {
                self.current.get(name).or_else(|| self.packuments.get(name))
            }
            Freshness::MayBeCached => self.packuments.get(name),
        };
        let answer = view
            .cloned()
            .ok_or_else(|| RegistryError::PackageNotFound(name.to_string()));
        InFlight::leave(&self.packuments_in_flight);

        answer
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

    /// The fixture never answers `304`, so a cache layered over it always
    /// takes the `200` path -- which is what makes the *window* the thing
    /// under test rather than the revalidation.
    ///
    /// A conditional request is one that reaches the registry, so it is
    /// answered from the current view: whatever has been published by now.
    fn packument_conditional(
        &self,
        name: &str,
        _etag: Option<&str>,
    ) -> Result<Fetched, RegistryError> {
        self.answer(name, Freshness::MustBeCurrent)
            .map(|packument| Fetched::Body {
                packument: Box::new(packument),
                etag: None,
            })
    }

    /// Overridden rather than left to the default, which discards its
    /// `freshness` argument.
    ///
    /// That default is right for a client with nothing stored -- it is already
    /// as current as it can be -- but it makes a fixture unable to tell the
    /// two questions apart, and a resolver that answered a dist-tag from a
    /// copy taken for a range would pass every test written against it. See
    /// [`FixtureRegistry::with_current_version`].
    fn packument(&self, name: &str, freshness: Freshness) -> Result<Packument, RegistryError> {
        self.answer(name, freshness)
    }

    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>, RegistryError> {
        self.tarball_calls.fetch_add(1, Ordering::Relaxed);
        if let Some(name) = self.name_of_tarball(url) {
            *self
                .tarball_calls_by_name
                .lock()
                .unwrap()
                .entry(name)
                .or_insert(0) += 1;
        }
        if let Some(handshake) = &self.packument_handshake {
            handshake.open();
        }
        InFlight::enter(&self.tarballs_in_flight);
        if let Some(rendezvous) = &self.tarball_rendezvous {
            rendezvous.meet();
        }
        let answer = self
            .tarballs
            .get(url)
            .cloned()
            .ok_or_else(|| RegistryError::PackageNotFound(url.to_string()));
        InFlight::leave(&self.tarballs_in_flight);

        answer
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

        let p = registry.packument("a", Freshness::MayBeCached).unwrap();
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
    fn latest_does_not_point_at_a_prerelease() {
        // A registry does not tag a prerelease `latest`, and a fixture that
        // did would make every test registering one exercise behaviour npm
        // does not have — silently, since `latest` is what a bare
        // `jerky install` asks for.
        let registry =
            FixtureRegistry::new().with_packument("a", &[("4.17.21", &[]), ("5.0.0-beta.1", &[])]);

        assert_eq!(
            registry
                .packument("a", Freshness::MayBeCached)
                .unwrap()
                .resolve_tag("latest"),
            Some("4.17.21")
        );
    }

    #[test]
    fn latest_falls_back_when_every_version_is_a_prerelease() {
        // Nothing stable to point at, and an unresolvable `latest` would be a
        // worse fixture than an imprecise one. A real registry does the same.
        let registry = FixtureRegistry::new()
            .with_packument("a", &[("1.0.0-alpha.1", &[]), ("1.0.0-alpha.2", &[])]);

        assert_eq!(
            registry
                .packument("a", Freshness::MayBeCached)
                .unwrap()
                .resolve_tag("latest"),
            Some("1.0.0-alpha.2")
        );
    }

    #[test]
    fn fixture_versions_verify_against_their_own_tarballs() {
        // The point of deriving integrity from the generated bytes: the happy
        // path must verify by construction, or every install test is testing
        // the failure path by accident.
        let registry = FixtureRegistry::new().with_packument("a", &[("1.0.0", &[])]);

        let p = registry.packument("a", Freshness::MayBeCached).unwrap();
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
            registry
                .packument("a", Freshness::MayBeCached)
                .unwrap()
                .resolve_tag("latest"),
            Some("2.0.0")
        );
    }

    #[test]
    fn packument_calls_are_counted_per_package() {
        let registry = FixtureRegistry::new()
            .with_packument("a", &[("1.0.0", &[])])
            .with_packument("b", &[("1.0.0", &[])]);

        registry.packument("a", Freshness::MayBeCached).unwrap();
        registry.packument("a", Freshness::MayBeCached).unwrap();
        registry.packument("b", Freshness::MayBeCached).unwrap();

        assert_eq!(registry.packument_calls_for("a"), 2);
        assert_eq!(registry.packument_calls_for("b"), 1);
        assert_eq!(registry.packument_calls(), 3, "the total is the sum");
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;
    use crate::registry::{Freshness, RegistryClient};

    #[test]
    fn with_tree_collects_repeated_names_into_one_packument() {
        // Two versions of `d` listed separately must not clobber each other:
        // a registry holds both, and the conflict test depends on it.
        let registry = FixtureRegistry::new().with_tree(&[
            ("d", "1.5.0", &[]),
            ("d", "2.1.0", &[]),
            ("a", "1.0.0", &[("d", "^1.0.0")]),
        ]);

        let d = registry.packument("d", Freshness::MayBeCached).unwrap();
        assert_eq!(d.versions.len(), 2);
        assert_eq!(d.resolve_tag("latest"), Some("2.1.0"));

        let a = registry.packument("a", Freshness::MayBeCached).unwrap();
        assert_eq!(a.versions["1.0.0"].dependencies["d"], "^1.0.0");
    }
}
