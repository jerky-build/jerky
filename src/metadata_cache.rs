//! Registry metadata on disk, with a freshness window.
//!
//! jerky held no metadata between runs, so every resolution fetched every
//! packument from the network. Measured on the `alotta-packages` fixture —
//! 2238 packuments — that walk is ~9s, of which the bytes are ~1s and parsing
//! all 470 MB of them is 0.45s. The cost is round trips, which is why this
//! stores whole packuments against a window rather than merely revalidating
//! them: a conditional request saves 99.85% of the bytes and about 11% of the
//! time, and only *not asking at all* collapses the row.
//!
//! What is stored is jerky's parsed [`Packument`], not the registry's bytes.
//! It is smaller — the abbreviated document carries fields nothing here reads
//! — and `Packument`'s `BTreeMap`s make the re-serialization deterministic.
//! The cost is that adding a field to the parsed form makes every existing
//! entry wrong, which is what `CACHE_VERSION` is for: bump it and old entries
//! are ignored rather than misread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::registry::{
    Fetched, Freshness, Packument, RegistryClient, RegistryError, VersionMetadata,
};

/// Bumped when the stored shape changes. Entries under an older version are
/// ignored, never reinterpreted.
const CACHE_VERSION: &str = "v1";

/// Distinguishes temporary files written by threads of one process. Paired
/// with the process id, which distinguishes processes.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How long an entry answers without asking the registry.
///
/// A day, which is roughly what npm and pnpm ship. The drift this admits is
/// real and bounded: a version published inside the window is invisible to a
/// range resolved from a cached entry until the window lapses. It is
/// acceptable here because jerky pins exact by default and the lockfile
/// records what was chosen, so a stale resolution is reproducible and visible
/// in a diff rather than silent.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("could not write the metadata cache entry for `{name}`")]
    Write {
        name: String,
        #[source]
        source: std::io::Error,
    },
}

/// What a lookup found.
#[derive(Debug)]
pub enum Lookup {
    /// Inside the window. Usable without asking the registry.
    ///
    /// Carries the ETag even though a fresh entry needs no revalidation: a
    /// dist-tag must reach the registry however fresh the copy is, and without
    /// the ETag that request could only be unconditional — turning "asks every
    /// time" into "re-downloads every time".
    Fresh {
        packument: Box<Packument>,
        etag: Option<String>,
    },
    /// On disk but past the window. Carries what a conditional request needs,
    /// and how old it is so a refusal can say.
    Stale {
        packument: Box<Packument>,
        etag: Option<String>,
        age: Duration,
    },
    /// Nothing usable on disk.
    Missing,
}

/// One entry as it sits on disk.
#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// Seconds since the epoch. Stored rather than read from the file's mtime
    /// because an mtime is rewritten by things that are not jerky — a backup
    /// restore, a `cp -r` of a home directory — and the window has to mean
    /// "when the registry last confirmed this", not "when this file last
    /// moved".
    fetched_at: u64,
    etag: Option<String>,
    packument: Packument,
}

/// Packuments on disk under a versioned root.
#[derive(Debug, Clone)]
pub struct MetadataCache {
    root: PathBuf,
    window: Duration,
}

impl MetadataCache {
    pub fn new(root: impl Into<PathBuf>, window: Duration) -> Self {
        Self {
            root: root.into(),
            window,
        }
    }

    fn versioned_root(&self) -> PathBuf {
        self.root.join(CACHE_VERSION)
    }

    /// Where one package's entry lives.
    ///
    /// The `/` in a scoped name is escaped rather than nested, so the path is
    /// always exactly one segment below the root. That is what makes a hostile
    /// name harmless: `../../etc/passwd` escapes nothing once its separators
    /// are gone, and no amount of `..` in a single filename leaves the
    /// directory. Names arrive from a manifest, which may have been
    /// hand-edited, so this is the untrusted-input boundary for the cache.
    fn entry_path(&self, name: &str) -> PathBuf {
        self.versioned_root().join(format!("{}.json", encode(name)))
    }

    /// What is on disk for this package, judged against the window.
    ///
    /// Every failure to read is [`Lookup::Missing`] rather than an error. A
    /// cache is an optimisation: a truncated file, an entry written by a
    /// future version, a permissions problem — none of them are reasons to
    /// fail an install that can still reach the registry.
    pub fn get(&self, name: &str) -> Lookup {
        let Ok(raw) = std::fs::read_to_string(self.entry_path(name)) else {
            return Lookup::Missing;
        };
        let Ok(entry) = serde_json::from_str::<Entry>(&raw) else {
            return Lookup::Missing;
        };

        match age_of(entry.fetched_at) {
            // A future timestamp means a clock moved backwards, or someone
            // edited the file. Treat it as stale rather than as infinitely
            // fresh: revalidating costs one round trip, and trusting it could
            // pin a stale entry until the clock catches up.
            None => Lookup::Stale {
                packument: Box::new(entry.packument),
                etag: entry.etag,
                age: Duration::ZERO,
            },
            Some(age) if age < self.window => Lookup::Fresh {
                packument: Box::new(entry.packument),
                etag: entry.etag,
            },
            Some(age) => Lookup::Stale {
                packument: Box::new(entry.packument),
                etag: entry.etag,
                age,
            },
        }
    }

    /// Record a packument as current as of now.
    pub fn put(
        &self,
        name: &str,
        packument: &Packument,
        etag: Option<&str>,
    ) -> Result<(), CacheError> {
        let entry = Entry {
            fetched_at: now_secs(),
            etag: etag.map(str::to_string),
            // Cloned rather than borrowed: `Entry` is also what `get`
            // deserializes into, and one shape for both directions is worth
            // more than saving a clone on a path that is about to do file I/O.
            packument: packument.clone(),
        };
        let body = serde_json::to_vec(&entry).expect("a packument serializes");
        self.write(name, &body)
    }

    /// Write via a temporary file and rename, so a killed process leaves
    /// either the old entry or the new one and never half of either.
    ///
    /// The temporary name carries the process id and a counter, so two jerky
    /// processes installing at once — two terminals, or two CI jobs sharing a
    /// home — cannot write the same temporary file. Sharing it would let their
    /// bytes interleave into something that then gets renamed over the live
    /// entry. `get` would treat the result as a miss rather than misread it,
    /// so the cost is a wasted fetch rather than a wrong resolution, but a
    /// unique name is cheaper than relying on that.
    fn write(&self, name: &str, body: &[u8]) -> Result<(), CacheError> {
        let failed = |source: std::io::Error| CacheError::Write {
            name: name.to_string(),
            source,
        };

        create_dirs_at_0o755(&self.versioned_root()).map_err(failed)?;

        let final_path = self.entry_path(name);
        let temp_path = final_path.with_extension(format!(
            "json.{}.{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&temp_path, body).map_err(failed)?;
        set_mode(&temp_path, 0o644).map_err(failed)?;
        std::fs::rename(&temp_path, &final_path).map_err(failed)?;
        Ok(())
    }
}

/// A package name as one filename.
///
/// `%` is escaped before `/` so the mapping is injective: escaping only the
/// separator would give `@a/b` and the literal name `@a%2fb` the same entry,
/// and one package's packument would answer for the other. npm rejects `%` in
/// a name, so this is unreachable through the registry — but names also arrive
/// from a hand-edited manifest, and "the registry would not allow it" is not a
/// property of the input this sees.
fn encode(name: &str) -> String {
    name.replace('%', "%25").replace('/', "%2f")
}

/// `create_dir_all` takes its mode from the process umask, so under a
/// permissive one every level it creates is world-writable — including
/// `~/.jerky` and `~/.jerky/cache`, not merely the versioned leaf. This cache
/// is `$HOME`-wide and shared by every project on the machine exactly as the
/// store is, so a directory another user can write into is one they can put a
/// packument in, and every project on the box resolves from it.
///
/// Only what was actually missing is touched. A level that already existed
/// belongs to whoever made it, and rewriting its mode would be this function
/// deciding something about a directory it did not create.
///
/// This is the fourth copy of this rule — `archive::normalise_created_dirs`,
/// `staging`, and `linker::create_dirs_at_0o755` are the others. Worth hoisting
/// into one place; not done here because the three of them return three
/// different error types and this change has no other business in those
/// modules.
fn create_dirs_at_0o755(dir: &Path) -> std::io::Result<()> {
    let missing: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|level| !level.exists())
        .map(Path::to_path_buf)
        .collect();

    std::fs::create_dir_all(dir)?;

    for level in missing {
        set_mode(&level, 0o755)?;
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How long ago `stamp` was, or `None` if it is in the future.
fn age_of(stamp: u64) -> Option<Duration> {
    now_secs().checked_sub(stamp).map(Duration::from_secs)
}

/// A [`RegistryClient`] that answers from disk while an entry is inside the
/// freshness window.
///
/// Wraps any client rather than `HttpRegistry` specifically, so the policy
/// here can be driven by a fixture in tests — the same reason
/// [`RegistryClient`] is a trait at all.
pub struct CachedRegistry<R> {
    inner: R,
    cache: MetadataCache,
}

impl<R: RegistryClient> CachedRegistry<R> {
    pub fn new(inner: R, cache: MetadataCache) -> Self {
        Self { inner, cache }
    }

    /// The client underneath, so a test can ask what actually reached it.
    pub fn inner(&self) -> &R {
        &self.inner
    }

    /// A failure to write is a warning, never an error.
    ///
    /// The install has the packument in hand and can finish; a full disk or a
    /// read-only `$HOME` should make the next run slower, not make this one
    /// fail. Said out loud rather than swallowed, because a cache that is
    /// silently never written is indistinguishable from one that is not
    /// helping.
    fn store(&self, name: &str, packument: &Packument, etag: Option<&str>) {
        if let Err(err) = self.cache.put(name, packument, etag) {
            eprintln!("warning: {err}");
        }
    }
}

impl<R: RegistryClient> RegistryClient for CachedRegistry<R> {
    fn packument(&self, name: &str, freshness: Freshness) -> Result<Packument, RegistryError> {
        let lookup = self.cache.get(name);

        // Inside the window, and the caller did not demand the registry: this
        // is the case the cache exists for, and it makes no request at all.
        if let (Lookup::Fresh { packument, .. }, Freshness::MayBeCached) = (&lookup, freshness) {
            return Ok((**packument).clone());
        }

        let (stored, etag, age) = match lookup {
            Lookup::Fresh { packument, etag } => (Some(packument), etag, None),
            Lookup::Stale {
                packument,
                etag,
                age,
            } => (Some(packument), etag, Some(age)),
            Lookup::Missing => (None, None, None),
        };

        match self.inner.packument_conditional(name, etag.as_deref()) {
            Ok(Fetched::Body { packument, etag }) => {
                self.store(name, &packument, etag.as_deref());
                Ok(*packument)
            }
            // The registry confirmed the stored copy just now. Re-stamping it
            // buys a whole new window without moving a byte of body.
            // A `304` is only a valid answer to a request that carried an
            // ETag, and one is sent only when something was on disk. A
            // registry answering it anyway is misbehaving, which is reported
            // rather than panicked — the same call this trait's default
            // `packument` makes for the same situation.
            Ok(Fetched::NotModified) => match stored {
                Some(packument) => {
                    self.store(name, &packument, etag.as_deref());
                    Ok(*packument)
                }
                None => Err(RegistryError::MalformedResponse {
                    url: name.to_string(),
                    source: "the registry answered 304 to a request with no ETag".into(),
                }),
            },
            // Unreachable, with something usable on disk that is past its
            // window. Refused rather than served: the window is what bounds
            // how stale an answer may be, and serving a lapsed entry because
            // the network happened to be down makes that bound unenforceable
            // exactly when nobody is watching.
            Err(err) => Err(match age {
                Some(age) => RegistryError::StaleCacheOnly {
                    name: name.to_string(),
                    age,
                    source: Box::new(err),
                },
                None => err,
            }),
        }
    }

    fn packument_conditional(
        &self,
        name: &str,
        etag: Option<&str>,
    ) -> Result<Fetched, RegistryError> {
        self.inner.packument_conditional(name, etag)
    }

    fn version_metadata(
        &self,
        name: &str,
        version: &str,
    ) -> Result<VersionMetadata, RegistryError> {
        self.inner.version_metadata(name, version)
    }

    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>, RegistryError> {
        self.inner.fetch_tarball(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    fn packument(name: &str, version: &str) -> Packument {
        serde_json::from_str(&format!(
            r#"{{"name":"{name}","dist-tags":{{"latest":"{version}"}},
                "versions":{{"{version}":{{"name":"{name}","version":"{version}",
                "dist":{{"tarball":"https://r.test/a.tgz"}}}}}}}}"#
        ))
        .unwrap()
    }

    /// Write an entry with an explicit timestamp, which is the only way to
    /// test a window without waiting a day.
    fn put_aged(
        cache: &MetadataCache,
        name: &str,
        p: &Packument,
        etag: Option<&str>,
        age: Duration,
    ) {
        let entry = Entry {
            fetched_at: now_secs() - age.as_secs(),
            etag: etag.map(str::to_string),
            packument: p.clone(),
        };
        cache
            .write(name, &serde_json::to_vec(&entry).unwrap())
            .unwrap();
    }

    #[test]
    fn a_miss_is_a_miss() {
        let dir = TempDir::new().unwrap();
        let cache = MetadataCache::new(dir.path(), DEFAULT_WINDOW);
        assert!(matches!(cache.get("lodash"), Lookup::Missing));
    }

    #[test]
    fn what_was_put_comes_back_fresh() {
        let dir = TempDir::new().unwrap();
        let cache = MetadataCache::new(dir.path(), DEFAULT_WINDOW);
        cache
            .put("lodash", &packument("lodash", "4.17.21"), Some("\"abc\""))
            .unwrap();

        let Lookup::Fresh {
            packument: found, ..
        } = cache.get("lodash")
        else {
            panic!("a just-written entry is inside any window");
        };
        assert_eq!(found.name, "lodash");
        assert_eq!(found.resolve_tag("latest"), Some("4.17.21"));
    }

    #[test]
    fn an_entry_past_the_window_is_stale_and_keeps_its_etag() {
        // The whole point of storing the ETag: a stale entry is what a
        // conditional request is made *from*, so losing it would turn every
        // lapse into a full re-download.
        let dir = TempDir::new().unwrap();
        let cache = MetadataCache::new(dir.path(), Duration::from_secs(3600));
        put_aged(
            &cache,
            "lodash",
            &packument("lodash", "4.17.21"),
            Some("\"abc\""),
            Duration::from_secs(7200),
        );

        let Lookup::Stale { etag, age, .. } = cache.get("lodash") else {
            panic!("two hours is outside a one-hour window");
        };
        assert_eq!(etag.as_deref(), Some("\"abc\""));
        assert!(age >= Duration::from_secs(7200), "age was {age:?}");
    }

    #[test]
    fn the_window_boundary_is_exclusive() {
        // An entry exactly as old as the window revalidates rather than
        // answering. The alternative reads "fresh for 24 hours *and a bit*",
        // which is a promise nothing states.
        let dir = TempDir::new().unwrap();
        let window = Duration::from_secs(3600);
        let cache = MetadataCache::new(dir.path(), window);
        put_aged(&cache, "a", &packument("a", "1.0.0"), None, window);

        assert!(matches!(cache.get("a"), Lookup::Stale { .. }));
    }

    #[test]
    fn refreshing_makes_a_stale_entry_fresh_again() {
        // What a 304 does. The body is unchanged; only the stamp moves.
        let dir = TempDir::new().unwrap();
        let cache = MetadataCache::new(dir.path(), Duration::from_secs(3600));
        let p = packument("lodash", "4.17.21");
        put_aged(
            &cache,
            "lodash",
            &p,
            Some("\"abc\""),
            Duration::from_secs(7200),
        );
        assert!(matches!(cache.get("lodash"), Lookup::Stale { .. }));

        cache.put("lodash", &p, Some("\"abc\"")).unwrap();

        assert!(matches!(cache.get("lodash"), Lookup::Fresh { .. }));
    }

    #[test]
    fn a_future_timestamp_is_stale_rather_than_eternally_fresh() {
        // A clock that moved backwards, or an edited file. Revalidating costs
        // one round trip; trusting it would pin the entry until the clock
        // caught up.
        let dir = TempDir::new().unwrap();
        let cache = MetadataCache::new(dir.path(), DEFAULT_WINDOW);
        let entry = Entry {
            fetched_at: now_secs() + 86_400,
            etag: None,
            packument: packument("a", "1.0.0"),
        };
        cache
            .write("a", &serde_json::to_vec(&entry).unwrap())
            .unwrap();

        assert!(matches!(cache.get("a"), Lookup::Stale { .. }));
    }

    #[test]
    fn a_corrupt_entry_is_a_miss_not_a_failure() {
        // A cache is an optimisation. A truncated file is a reason to ask the
        // registry, never a reason to fail an install that could have asked.
        let dir = TempDir::new().unwrap();
        let cache = MetadataCache::new(dir.path(), DEFAULT_WINDOW);
        cache.write("lodash", b"{not json").unwrap();

        assert!(matches!(cache.get("lodash"), Lookup::Missing));
    }

    #[test]
    fn a_scoped_name_stays_one_segment_below_the_root() {
        let dir = TempDir::new().unwrap();
        let cache = MetadataCache::new(dir.path(), DEFAULT_WINDOW);
        cache
            .put("@types/node", &packument("@types/node", "20.1.0"), None)
            .unwrap();

        let path = cache.entry_path("@types/node");
        assert_eq!(path.parent().unwrap(), cache.versioned_root());
        assert!(matches!(cache.get("@types/node"), Lookup::Fresh { .. }));
    }

    #[test]
    fn a_name_full_of_traversal_escapes_nothing() {
        // Names reach here from a manifest, which may have been hand-edited.
        // Escaping the separator is what contains this: no quantity of `..`
        // inside a single filename leaves the directory it sits in.
        let dir = TempDir::new().unwrap();
        let cache = MetadataCache::new(dir.path(), DEFAULT_WINDOW);

        for hostile in ["../../etc/passwd", "..", "a/../../b"] {
            let path = cache.entry_path(hostile);
            assert_eq!(
                path.parent().unwrap(),
                cache.versioned_root(),
                "`{hostile}` left the cache root"
            );
        }

        cache
            .put("../../etc/passwd", &packument("x", "1.0.0"), None)
            .unwrap();
        assert!(
            dir.path().join(CACHE_VERSION).read_dir().unwrap().count() == 1,
            "the write landed somewhere other than the cache root"
        );
    }

    #[test]
    fn entries_are_not_group_or_world_writable() {
        // `$HOME`-wide and shared by every project on the machine, exactly as
        // the store is. A writable entry here is a packument every project
        // resolves from.
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        // A root several levels below one that exists, so this exercises what
        // the real `~/.jerky/cache/v1` does: `create_dir_all` makes every
        // level, and under a permissive umask each would be 0o777. Rooting the
        // cache directly at the `TempDir` would create only the leaf and prove
        // almost nothing — and the `TempDir` itself is deliberately excluded
        // below, being a directory jerky did not create.
        let existing = dir.path();
        let root = existing.join("nested").join("cache");
        let cache = MetadataCache::new(&root, DEFAULT_WINDOW);
        cache.put("a", &packument("a", "1.0.0"), None).unwrap();

        let file = std::fs::metadata(cache.entry_path("a")).unwrap();
        assert_eq!(file.permissions().mode() & 0o777, 0o644);

        let mut level = cache.versioned_root();
        while level != existing {
            let mode = std::fs::metadata(&level).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "{} was {mode:o}", level.display());
            level = level.parent().expect("stops at the temp dir").to_path_buf();
        }
    }

    #[test]
    fn a_directory_that_already_existed_keeps_its_own_mode() {
        // A level jerky did not create belongs to whoever did, and rewriting
        // its mode would be this deciding something about a directory it does
        // not own.
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("cache");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();

        let cache = MetadataCache::new(&root, DEFAULT_WINDOW);
        cache.put("a", &packument("a", "1.0.0"), None).unwrap();

        let mode = std::fs::metadata(&root).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "a pre-existing directory was rewritten"
        );
    }

    #[test]
    fn encoding_a_name_is_injective() {
        // Escaping only the separator would give `@a/b` and the literal name
        // `@a%2fb` one entry between them, so one package's packument would
        // answer for the other.
        assert_ne!(encode("@a/b"), encode("@a%2fb"));
    }

    #[test]
    fn concurrent_writers_do_not_share_a_temporary_file() {
        // Two processes sharing a home, or two threads of one. Interleaving
        // into a single temporary file and renaming the result over the live
        // entry is the failure this guards; `get` would call the result a miss
        // rather than misread it, but a wasted fetch is still a cost.
        let dir = TempDir::new().unwrap();
        let cache = MetadataCache::new(dir.path(), DEFAULT_WINDOW);
        let p = packument("a", "1.0.0");

        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..25 {
                        cache.put("a", &p, Some("\"e\"")).unwrap();
                    }
                });
            }
        });

        // Whatever interleaving happened, the live entry is readable.
        assert!(matches!(cache.get("a"), Lookup::Fresh { .. }));
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        let dir = TempDir::new().unwrap();
        let cache = MetadataCache::new(dir.path(), DEFAULT_WINDOW);
        cache.put("a", &packument("a", "1.0.0"), None).unwrap();

        let leftovers: Vec<_> = cache
            .versioned_root()
            .read_dir()
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::TempDir;

    /// A registry that counts what it was asked and can be told to fail, so a
    /// test can assert on the number of requests rather than on timings.
    struct Fake {
        packument: Mutex<Packument>,
        etag: Mutex<Option<String>>,
        /// Requests that arrived carrying an `If-None-Match`.
        conditional: AtomicUsize,
        /// Requests that arrived without one.
        unconditional: AtomicUsize,
        /// When set, the ETag the fake treats as still current: a matching
        /// request is answered `304`.
        current_etag: Mutex<Option<String>>,
        offline: AtomicUsize,
    }

    impl Fake {
        fn new(p: Packument) -> Self {
            Self {
                packument: Mutex::new(p),
                etag: Mutex::new(Some("\"v1\"".into())),
                conditional: AtomicUsize::new(0),
                unconditional: AtomicUsize::new(0),
                current_etag: Mutex::new(None),
                offline: AtomicUsize::new(0),
            }
        }

        fn requests(&self) -> usize {
            self.conditional.load(Ordering::Relaxed) + self.unconditional.load(Ordering::Relaxed)
        }

        fn go_offline(&self) {
            self.offline.store(1, Ordering::Relaxed);
        }

        /// Answer `304` to anyone presenting this ETag.
        fn treat_as_current(&self, etag: &str) {
            *self.current_etag.lock().unwrap() = Some(etag.to_string());
        }
    }

    impl RegistryClient for Fake {
        fn packument_conditional(
            &self,
            name: &str,
            etag: Option<&str>,
        ) -> Result<Fetched, RegistryError> {
            match etag {
                Some(_) => self.conditional.fetch_add(1, Ordering::Relaxed),
                None => self.unconditional.fetch_add(1, Ordering::Relaxed),
            };

            if self.offline.load(Ordering::Relaxed) == 1 {
                return Err(RegistryError::Network {
                    url: name.to_string(),
                    source: "network is unreachable".into(),
                });
            }

            if let (Some(sent), Some(current)) = (etag, self.current_etag.lock().unwrap().as_ref())
                && sent == current
            {
                return Ok(Fetched::NotModified);
            }

            Ok(Fetched::Body {
                packument: Box::new(self.packument.lock().unwrap().clone()),
                etag: self.etag.lock().unwrap().clone(),
            })
        }

        fn version_metadata(
            &self,
            _name: &str,
            _version: &str,
        ) -> Result<VersionMetadata, RegistryError> {
            unimplemented!("not exercised by the cache")
        }

        fn fetch_tarball(&self, _url: &str) -> Result<Vec<u8>, RegistryError> {
            unimplemented!("not exercised by the cache")
        }
    }

    fn packument(name: &str, version: &str) -> Packument {
        serde_json::from_str(&format!(
            r#"{{"name":"{name}","dist-tags":{{"latest":"{version}"}},
                "versions":{{"{version}":{{"name":"{name}","version":"{version}",
                "dist":{{"tarball":"https://r.test/a.tgz"}}}}}}}}"#
        ))
        .unwrap()
    }

    fn cached(dir: &TempDir, window: Duration, fake: Fake) -> CachedRegistry<Fake> {
        CachedRegistry::new(fake, MetadataCache::new(dir.path(), window))
    }

    #[test]
    fn a_range_inside_the_window_makes_no_request_at_all() {
        // The row this whole change exists for: ~9s of round trips on
        // alotta-packages becomes none.
        let dir = TempDir::new().unwrap();
        let registry = cached(
            &dir,
            DEFAULT_WINDOW,
            Fake::new(packument("lodash", "4.17.21")),
        );

        registry
            .packument("lodash", Freshness::MayBeCached)
            .unwrap();
        assert_eq!(registry.inner.requests(), 1, "the first ask must fetch");

        for _ in 0..5 {
            let p = registry
                .packument("lodash", Freshness::MayBeCached)
                .unwrap();
            assert_eq!(p.name, "lodash");
        }
        assert_eq!(
            registry.inner.requests(),
            1,
            "every later ask inside the window must be answered from disk"
        );
    }

    #[test]
    fn a_dist_tag_always_asks_even_with_a_fresh_entry() {
        // The promise the CHANGELOG makes: only the registry can say what
        // `latest` means today. A window must not quietly answer it.
        let dir = TempDir::new().unwrap();
        let registry = cached(
            &dir,
            DEFAULT_WINDOW,
            Fake::new(packument("lodash", "4.17.21")),
        );

        registry
            .packument("lodash", Freshness::MayBeCached)
            .unwrap();
        let after_seeding = registry.inner.requests();

        registry
            .packument("lodash", Freshness::MustBeCurrent)
            .unwrap();
        registry
            .packument("lodash", Freshness::MustBeCurrent)
            .unwrap();

        assert_eq!(
            registry.inner.requests(),
            after_seeding + 2,
            "a dist-tag must reach the registry every time"
        );
    }

    #[test]
    fn a_dist_tag_revalidates_rather_than_re_downloading() {
        // "Asks every time" need not mean "downloads every time". The request
        // is conditional, so the registry answers 304 and no body moves.
        let dir = TempDir::new().unwrap();
        let registry = cached(
            &dir,
            DEFAULT_WINDOW,
            Fake::new(packument("lodash", "4.17.21")),
        );
        registry
            .packument("lodash", Freshness::MayBeCached)
            .unwrap();
        registry.inner.treat_as_current("\"v1\"");

        registry
            .packument("lodash", Freshness::MustBeCurrent)
            .unwrap();

        assert_eq!(
            registry.inner.conditional.load(Ordering::Relaxed),
            1,
            "the revalidation must carry the stored ETag"
        );
    }

    #[test]
    fn a_lapsed_entry_revalidates_and_a_304_wins_a_new_window() {
        let dir = TempDir::new().unwrap();
        let fake = Fake::new(packument("lodash", "4.17.21"));
        let registry = cached(&dir, Duration::from_secs(0), fake);

        // Window of zero: everything is immediately stale.
        registry
            .packument("lodash", Freshness::MayBeCached)
            .unwrap();
        registry.inner.treat_as_current("\"v1\"");
        registry
            .packument("lodash", Freshness::MayBeCached)
            .unwrap();
        assert_eq!(
            registry.inner.conditional.load(Ordering::Relaxed),
            1,
            "a lapsed entry is revalidated, not re-downloaded"
        );

        // And the 304 re-stamped it, so a real window would now answer from
        // disk. Proved by widening the window and asking again.
        let widened = CachedRegistry::new(
            Fake::new(packument("lodash", "4.17.21")),
            MetadataCache::new(dir.path(), DEFAULT_WINDOW),
        );
        widened.packument("lodash", Freshness::MayBeCached).unwrap();
        assert_eq!(
            widened.inner.requests(),
            0,
            "the 304 should have stamped the entry fresh"
        );
    }

    #[test]
    fn a_changed_packument_replaces_the_cached_one() {
        let dir = TempDir::new().unwrap();
        let registry = cached(
            &dir,
            Duration::from_secs(0),
            Fake::new(packument("lodash", "4.17.21")),
        );
        registry
            .packument("lodash", Freshness::MayBeCached)
            .unwrap();

        *registry.inner.packument.lock().unwrap() = packument("lodash", "4.17.22");
        let updated = registry
            .packument("lodash", Freshness::MayBeCached)
            .unwrap();

        assert_eq!(updated.resolve_tag("latest"), Some("4.17.22"));
    }

    #[test]
    fn unreachable_with_a_lapsed_entry_refuses_and_says_how_old_it_is() {
        // The decision: the window is what bounds staleness, and serving a
        // lapsed entry because the network is down makes that bound
        // unenforceable exactly when nobody is watching.
        let dir = TempDir::new().unwrap();
        let registry = cached(
            &dir,
            Duration::from_secs(0),
            Fake::new(packument("lodash", "4.17.21")),
        );
        registry
            .packument("lodash", Freshness::MayBeCached)
            .unwrap();
        registry.inner.go_offline();

        let err = registry
            .packument("lodash", Freshness::MayBeCached)
            .expect_err("a lapsed entry must not answer while the registry is unreachable");

        let RegistryError::StaleCacheOnly { name, .. } = &err else {
            panic!("expected StaleCacheOnly, got {err}");
        };
        assert_eq!(name, "lodash");
        let rendered = err.to_string();
        assert!(rendered.contains("freshness window"), "{rendered}");
    }

    #[test]
    fn unreachable_with_nothing_cached_reports_the_network_failure_itself() {
        // No cached copy means there is nothing to say about staleness, and
        // dressing a plain network failure up as a cache problem would send
        // the reader looking in the wrong place.
        let dir = TempDir::new().unwrap();
        let registry = cached(&dir, DEFAULT_WINDOW, Fake::new(packument("a", "1.0.0")));
        registry.inner.go_offline();

        let err = registry
            .packument("a", Freshness::MayBeCached)
            .expect_err("nothing cached and no network");

        assert!(
            matches!(err, RegistryError::Network { .. }),
            "expected a plain network error, got {err}"
        );
    }

    #[test]
    fn a_304_to_a_request_that_sent_no_etag_is_reported_not_panicked() {
        // `CachedRegistry` is generic over `RegistryClient` precisely so other
        // clients can be wrapped, and a misbehaving one describes a peer at
        // fault rather than a bug here.
        let dir = TempDir::new().unwrap();
        let fake = Fake::new(packument("a", "1.0.0"));
        // Answers 304 to anything, including a request carrying no ETag.
        *fake.current_etag.lock().unwrap() = None;
        struct AlwaysNotModified;
        impl RegistryClient for AlwaysNotModified {
            fn packument_conditional(
                &self,
                _name: &str,
                _etag: Option<&str>,
            ) -> Result<Fetched, RegistryError> {
                Ok(Fetched::NotModified)
            }
            fn version_metadata(
                &self,
                _n: &str,
                _v: &str,
            ) -> Result<VersionMetadata, RegistryError> {
                unimplemented!()
            }
            fn fetch_tarball(&self, _url: &str) -> Result<Vec<u8>, RegistryError> {
                unimplemented!()
            }
        }
        drop(fake);

        let registry = CachedRegistry::new(
            AlwaysNotModified,
            MetadataCache::new(dir.path(), DEFAULT_WINDOW),
        );

        let err = registry
            .packument("a", Freshness::MayBeCached)
            .expect_err("a 304 with nothing cached has no body to fall back on");
        assert!(
            matches!(err, RegistryError::MalformedResponse { .. }),
            "got {err}"
        );
    }

    #[test]
    fn a_fresh_entry_survives_the_registry_going_away() {
        // The offline win that falls out of a window: inside it, no request is
        // made, so there is nothing to fail.
        let dir = TempDir::new().unwrap();
        let registry = cached(&dir, DEFAULT_WINDOW, Fake::new(packument("a", "1.0.0")));
        registry.packument("a", Freshness::MayBeCached).unwrap();
        registry.inner.go_offline();

        let p = registry.packument("a", Freshness::MayBeCached).unwrap();
        assert_eq!(p.name, "a");
    }
}
