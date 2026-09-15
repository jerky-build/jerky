use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::integrity::{Integrity, IntegrityError};
use crate::range::Version;

/// How many requests jerky has in flight at the registry at once.
///
/// One number shared by resolution and installation rather than one each,
/// because what it bounds is a property of the registry rather than of either
/// caller. It is *not* a bound on the two together — two pools of this width
/// would be twice it — and does not need to be: an install resolves to
/// completion before it fetches a single tarball, so only one of them is ever
/// running.
///
/// Both are latency-bound fan-out rather than a CPU workload, so the cap is
/// not tied to core count: a thread waiting on a socket is not competing for
/// anything. Sixteen is where the measured gain on a real tree flattens.
/// Installing express's 69 packages into a cold store from a matching lockfile
/// — the CI and fresh-clone case, where every tarball is fetched and no
/// metadata is — went from 3.36s serially to 0.40s at this width, and
/// resolving that tree went from 2.60s to 0.70s. It stays far below the point
/// where a registry starts treating one client as abusive. A cap exists at all
/// because a resolved graph can hold thousands of packages, and one thread per
/// package is how an install gets an IP rate-limited.
pub const MAX_CONCURRENT_FETCHES: usize = 16;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("package `{0}` was not found in the registry")]
    PackageNotFound(String),
    #[error("package `{name}` has no version `{version}`")]
    VersionNotFound { name: String, version: String },
    #[error("could not reach the registry at {url}")]
    Network {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    // Deliberately says what is on disk and how old it is, rather than only
    // that the network failed. The user's next decision is whether to wait for
    // a network or to change what they asked for, and the age of what jerky
    // already has is what informs it.
    #[error(
        "could not reach the registry for `{name}`, and the cached copy is \
         {}d {}h old — past the freshness window, so jerky will not resolve \
         from it",
        .age.as_secs() / 86_400,
        (.age.as_secs() % 86_400) / 3600
    )]
    StaleCacheOnly {
        name: String,
        age: std::time::Duration,
        #[source]
        source: Box<RegistryError>,
    },
    #[error("the registry returned a response jerky could not understand for {url}")]
    MalformedResponse {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// The subset of a version manifest jerky needs. Unknown fields are ignored,
/// so registry additions do not break deserialization.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VersionMetadata {
    pub name: String,
    pub version: String,
    pub dist: Dist,
    /// Runtime dependencies only.
    ///
    /// There is deliberately no `devDependencies` field. A dependency's dev
    /// dependencies must never be followed — doing so pulls in most of the
    /// registry — and a field that exists is a field someone will read.
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
}

/// Every published version of one package, in the registry's abbreviated form.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Packument {
    pub name: String,
    #[serde(default)]
    pub versions: BTreeMap<String, VersionMetadata>,
    #[serde(rename = "dist-tags", default)]
    pub dist_tags: BTreeMap<String, String>,
}

impl Packument {
    /// Every version that parses, in precedence order.
    ///
    /// Unparseable versions are dropped rather than fatal: the registry has
    /// accumulated some genuinely malformed entries, and one of them must not
    /// make an otherwise-fine package unresolvable.
    ///
    /// Sorting matters. The registry's own key order is lexical, which puts
    /// `1.10.0` before `1.9.0`.
    pub fn versions_sorted(&self) -> Vec<Version> {
        let mut out: Vec<Version> = self
            .versions
            .keys()
            .filter_map(|v| Version::parse(v).ok())
            .collect();
        out.sort();
        out
    }

    /// Resolve a dist-tag such as `latest` to a concrete version.
    pub fn resolve_tag(&self, tag: &str) -> Option<&str> {
        self.dist_tags.get(tag).map(String::as_str)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Dist {
    pub tarball: String,
    #[serde(default)]
    pub integrity: Option<String>,
    #[serde(default)]
    pub shasum: Option<String>,
}

impl Dist {
    /// Prefer the modern `integrity` field, fall back to the legacy hex
    /// `shasum`, and report `Missing` when a package offers neither.
    pub fn integrity(&self) -> Result<Integrity, IntegrityError> {
        if let Some(raw) = &self.integrity {
            return Integrity::parse(raw);
        }
        if let Some(raw) = &self.shasum {
            return Integrity::from_shasum_hex(raw);
        }
        Err(IntegrityError::Missing)
    }
}

/// How current an answer has to be.
///
/// Carried by the caller rather than decided by the client, because only the
/// caller knows what was asked. A range names a set of versions and any
/// member of it that was valid a few hours ago is still a member; a dist-tag
/// names whatever the registry means by it *today*, so `latest` answered from
/// a day-old copy is a different question answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// A cache inside its freshness window may answer this without asking.
    MayBeCached,
    /// The registry must be asked. A conditional request satisfies this — a
    /// `304` is the registry stating, just now, that the copy is current.
    MustBeCurrent,
}

/// What a conditional fetch came back with.
#[derive(Debug)]
pub enum Fetched {
    /// The registry confirmed the caller's copy is current. No body.
    NotModified,
    Body {
        packument: Box<Packument>,
        etag: Option<String>,
    },
}

/// Everything jerky needs from a package registry.
///
/// The trait exists so the install pipeline can be driven by a fixture in
/// tests: offline, deterministic, and fast. Its cost is that the fixture
/// covers everything *except* the module it replaces, so `HttpRegistry` needs
/// its own tests.
///
/// `Sync` because install fetches tarballs from a pool of threads sharing one
/// client (#33). It is a bound on the trait rather than on that call site so
/// the requirement is stated where implementors read it: a client caching into
/// a bare `RefCell` would otherwise compile until the day it was shared.
pub trait RegistryClient: Sync {
    /// Fetch one version's manifest. `version` may be a concrete version or a
    /// dist-tag such as `latest`; the registry resolves both on this endpoint.
    fn version_metadata(&self, name: &str, version: &str)
    -> Result<VersionMetadata, RegistryError>;

    /// Every published version of a package, optionally conditionally.
    ///
    /// The primitive rather than a convenience: a cache needs to ask "is my
    /// copy still current" and get an answer cheaper than a body, and a client
    /// that could only return bodies would make the cache re-download
    /// everything it revalidated. Passing `None` always yields a `Body`.
    fn packument_conditional(
        &self,
        name: &str,
        etag: Option<&str>,
    ) -> Result<Fetched, RegistryError>;

    /// Every published version of a package, for range resolution.
    ///
    /// The default ignores `freshness` and always fetches, which is right for
    /// any client with nothing stored: it is already as current as it can be.
    /// Only a caching client overrides this.
    fn packument(&self, name: &str, freshness: Freshness) -> Result<Packument, RegistryError> {
        let _ = freshness;
        match self.packument_conditional(name, None)? {
            Fetched::Body { packument, .. } => Ok(*packument),
            // Unreachable over HTTP — a server may only answer `304` to a
            // conditional request, and none was made. Reported rather than
            // panicked because it describes a peer misbehaving, not a bug here.
            Fetched::NotModified => Err(RegistryError::MalformedResponse {
                url: name.to_string(),
                source: "the registry answered 304 to an unconditional request".into(),
            }),
        }
    }

    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>, RegistryError>;
}

pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

const MAX_ATTEMPTS: u32 = 3;

/// The first sleep after a transport failure or a 5xx, doubled each attempt:
/// 100ms, then 200ms. Short because the registry has not asked for anything —
/// a 503 is a fault, and faults at this layer are usually over in a moment.
const TRANSIENT_BACKOFF: Duration = Duration::from_millis(100);

/// The first sleep after a 429 that carried no `Retry-After`, doubled each
/// attempt: 1s, then 2s.
///
/// Ten times the transient step, because a 429 is not a fault but an
/// instruction, and the instruction is to send less. Registries that do
/// advertise a delay measure it in seconds, so a client guessing in their
/// absence should guess on that scale rather than on the scale of a hiccup.
///
/// Bounded deliberately at three attempts, which makes the worst case three
/// seconds of sleeping rather than a client that sits on a thread for minutes:
/// a rate limit that is not over in three seconds is a condition to report to
/// the user, not one to outwait.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(1);

/// The longest `Retry-After` jerky honours before giving up on waiting.
///
/// A registry asking for half an hour is asking for more than a command-line
/// tool can give it while someone watches; past this the wait would cost more
/// than the failure it is avoiding, and the honest answer is the error.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// ureq's `read_to_vec`/`read_to_string` default to a 10 MB cap, which many
/// real tarballs exceed, so both limits are raised explicitly. A version
/// manifest is a few KB; the generous ceiling is for pathological ones.
///
/// Exceeding the limit is an error (`BodyExceedsLimit`), not a truncation, so
/// the failure mode of leaving these at the default would be a loud refusal to
/// install a large package rather than a corrupted one. The caps stay because
/// an unbounded read lets a hostile registry exhaust memory.
///
/// Note that ureq rejects a body of *exactly* the limit: its `LimitReader`
/// errors once the remaining allowance reaches zero rather than when it would
/// go negative. Harmless at these sizes, but it is why the ceilings are round
/// numbers well clear of any real payload rather than tight fits.
const MAX_METADATA_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TARBALL_BYTES: u64 = 512 * 1024 * 1024;

/// The npm registry over HTTP.
pub struct HttpRegistry {
    base_url: String,
    agent: ureq::Agent,
}

impl Default for HttpRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// What jerky should do about a failed attempt, and how long to wait first.
enum Retry {
    /// A definite answer, or a wait longer than it is worth: stop.
    No,
    /// A transport failure or a 5xx. Nobody asked for anything, so retry
    /// promptly.
    Transient,
    /// The registry answered 429. `Some` carries the `Retry-After` it
    /// advertised; `None` means it asked for less traffic without saying how
    /// much less.
    RateLimited(Option<Duration>),
}

impl Retry {
    /// How long to sleep before attempt `n + 1`, counting from zero.
    fn backoff(&self, n: u32) -> Duration {
        match self {
            // Never reached: `No` returns before anything sleeps.
            Retry::No => Duration::ZERO,
            Retry::Transient => TRANSIENT_BACKOFF * 2_u32.pow(n),
            // An advertised delay is not doubled. The registry named a time to
            // come back at, and coming back later than asked is not politer,
            // only slower.
            Retry::RateLimited(Some(advertised)) => *advertised,
            Retry::RateLimited(None) => RATE_LIMIT_BACKOFF * 2_u32.pow(n),
        }
    }
}

impl HttpRegistry {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_REGISTRY)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        // The idle pool is sized from the fan-out rather than left at ureq's
        // default of three per host, because jerky points all sixteen workers
        // at one host. A pool narrower than the fan-out means a worker that
        // finishes a request finds the pool full, drops its connection, and
        // pays a fresh TCP and TLS handshake for its next one — around 2,900
        // handshakes for a cold install of a large tree, which is both slow
        // and the connection-churn pattern registries rate-limit on.
        //
        // The two limits are the same number on purpose. Only one pool of
        // `MAX_CONCURRENT_FETCHES` workers runs at a time, so sixteen is every
        // connection jerky can have open at once however many hosts they are
        // spread over: an overall cap below the per-host one would evict
        // connections the per-host limit had just agreed to keep.
        //
        // Statuses are left on the response rather than raised as errors
        // because a 429 is only actionable with its headers in hand, and
        // ureq's conversion into `Error::StatusCode` drops them.
        let config = ureq::Agent::config_builder()
            .user_agent(concat!("jerky/", env!("CARGO_PKG_VERSION")))
            .max_idle_connections(MAX_CONCURRENT_FETCHES)
            .max_idle_connections_per_host(MAX_CONCURRENT_FETCHES)
            .http_status_as_error(false)
            .build();

        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            agent: config.into(),
        }
    }

    /// Scoped names carry a `/` that must be escaped in a path segment:
    /// `@types/node` is requested as `@types%2fnode`.
    fn encode_name(name: &str) -> String {
        name.replace('/', "%2f")
    }

    /// Run an idempotent read, retrying on the schedule the failure earns.
    ///
    /// `Retry::No` results short-circuit: a 404 is a definite answer, and
    /// retrying it only makes the tool slow at being wrong. The sleep between
    /// the rest parks this thread; a worker blocked on a rate limit is a
    /// worker not adding to it, which is the point.
    fn with_retries<T>(
        mut attempt: impl FnMut() -> Result<T, (RegistryError, Retry)>,
    ) -> Result<T, RegistryError> {
        let mut last = None;
        for n in 0..MAX_ATTEMPTS {
            match attempt() {
                Ok(value) => return Ok(value),
                Err((err, Retry::No)) => return Err(err),
                Err((err, retry)) => {
                    last = Some(err);
                    if n + 1 < MAX_ATTEMPTS {
                        std::thread::sleep(retry.backoff(n));
                    }
                }
            }
        }
        Err(last.expect("at least one attempt ran"))
    }

    /// Turn a response jerky did not want into an error and a retry policy.
    ///
    /// The one distinction that matters is 429 from everything else: a 5xx is
    /// a fault the registry would rather not have had, and a 429 is the
    /// registry telling this client what to do next.
    fn from_status(
        url: &str,
        response: &ureq::http::Response<ureq::Body>,
    ) -> (RegistryError, Retry) {
        let status = response.status();
        let err = RegistryError::Network {
            url: url.to_string(),
            source: Box::new(ureq::Error::StatusCode(status.as_u16())),
        };

        if status != 429 {
            return (err, Retry::Transient);
        }

        match Self::retry_after(response) {
            Some(delay) if delay > MAX_RETRY_AFTER => (err, Retry::No),
            advertised => (err, Retry::RateLimited(advertised)),
        }
    }

    /// The `Retry-After` delay, in seconds.
    ///
    /// The header may also carry an HTTP date. jerky does not read one: it
    /// would need a date parser and a clock it trusts to agree with the
    /// registry's, and the fallback — the rate-limit backoff — is the same
    /// scale as the delays registries send. An unparseable header is simply
    /// one that was not sent.
    fn retry_after(response: &ureq::http::Response<ureq::Body>) -> Option<Duration> {
        let seconds: u64 = response
            .headers()
            .get("retry-after")?
            .to_str()
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some(Duration::from_secs(seconds))
    }

    /// Fetch a JSON body, applying the retry policy and the size limit.
    ///
    /// The two metadata endpoints differ only in their URL, whether they ask
    /// for the abbreviated form, and what a 404 means, so everything else —
    /// retries, limits, error mapping — lives here rather than twice.
    fn get_json(
        &self,
        url: &str,
        abbreviated: bool,
        not_found: impl Fn() -> RegistryError,
    ) -> Result<String, RegistryError> {
        Self::with_retries(|| {
            let mut request = self.agent.get(url);
            if abbreviated {
                // The unabbreviated document for a popular package is
                // megabytes of every version ever published, so this is not
                // an optimisation.
                request = request.header("Accept", "application/vnd.npm.install-v1+json");
            }

            match request.call() {
                Ok(response) if response.status() == 404 => Err((not_found(), Retry::No)),
                Ok(response) if !response.status().is_success() => {
                    Err(Self::from_status(url, &response))
                }
                Ok(mut response) => response
                    .body_mut()
                    .with_config()
                    .limit(MAX_METADATA_BYTES)
                    .read_to_string()
                    .map_err(|source| {
                        (
                            RegistryError::MalformedResponse {
                                url: url.to_string(),
                                source: Box::new(source),
                            },
                            Retry::Transient,
                        )
                    }),
                Err(source) => Err((
                    RegistryError::Network {
                        url: url.to_string(),
                        source: Box::new(source),
                    },
                    Retry::Transient,
                )),
            }
        })
    }

    /// Fetch a packument, sending `If-None-Match` when the caller has an ETag.
    ///
    /// `Ok(None)` is a `304`, and it is checked before anything else. A 304 is
    /// not a success status, so a client that asked `is_success()` first would
    /// classify every confirmed-current entry as a failure and retry it —
    /// turning the cheapest answer the registry can give into three requests
    /// and an error, and making the cache re-download exactly what it had just
    /// revalidated.
    ///
    /// Every other status is classified rather than read. This endpoint is the
    /// one the cache revalidates through, so it is the one that meets a 429
    /// most often, and a rate-limit body parsed as metadata would hand the
    /// resolver a packument the registry never sent.
    ///
    /// Separate from `get_json` rather than another flag on it: this one needs
    /// a response header and a status code back, and threading two more
    /// out-parameters through the shared helper for the sake of one caller
    /// would make the simple endpoint read like the complicated one.
    fn get_packument(
        &self,
        url: &str,
        etag: Option<&str>,
        not_found: impl Fn() -> RegistryError,
    ) -> Result<Option<(String, Option<String>)>, RegistryError> {
        Self::with_retries(|| {
            let mut request = self
                .agent
                .get(url)
                // The unabbreviated document for a popular package is
                // megabytes of every version ever published, so this is not
                // an optimisation.
                .header("Accept", "application/vnd.npm.install-v1+json");
            if let Some(etag) = etag {
                request = request.header("If-None-Match", etag);
            }

            match request.call() {
                Ok(response) if response.status() == 304 => Ok(None),
                Ok(response) if response.status() == 404 => Err((not_found(), Retry::No)),
                Ok(response) if !response.status().is_success() => {
                    Err(Self::from_status(url, &response))
                }
                Ok(mut response) => {
                    let etag = response
                        .headers()
                        .get("etag")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    response
                        .body_mut()
                        .with_config()
                        .limit(MAX_METADATA_BYTES)
                        .read_to_string()
                        .map(|body| Some((body, etag)))
                        .map_err(|source| {
                            (
                                RegistryError::MalformedResponse {
                                    url: url.to_string(),
                                    source: Box::new(source),
                                },
                                Retry::Transient,
                            )
                        })
                }
                Err(source) => Err((
                    RegistryError::Network {
                        url: url.to_string(),
                        source: Box::new(source),
                    },
                    Retry::Transient,
                )),
            }
        })
    }

    /// Does this package exist at all? Used only on the error path, to turn a
    /// 404 into either `PackageNotFound` or `VersionNotFound`.
    fn package_exists(&self, name: &str) -> bool {
        let url = format!("{}/{}", self.base_url, Self::encode_name(name));
        self.agent
            .get(&url)
            .call()
            .is_ok_and(|response| response.status().is_success())
    }
}

impl RegistryClient for HttpRegistry {
    fn version_metadata(
        &self,
        name: &str,
        version: &str,
    ) -> Result<VersionMetadata, RegistryError> {
        let url = format!("{}/{}/{}", self.base_url, Self::encode_name(name), version);

        // A 404 here is ambiguous: the package may not exist, or it may exist
        // without this version. Only the error path pays for the distinction.
        let body = self.get_json(&url, false, || {
            if self.package_exists(name) {
                RegistryError::VersionNotFound {
                    name: name.to_string(),
                    version: version.to_string(),
                }
            } else {
                RegistryError::PackageNotFound(name.to_string())
            }
        })?;

        serde_json::from_str(&body).map_err(|source| RegistryError::MalformedResponse {
            url,
            source: Box::new(source),
        })
    }

    fn packument_conditional(
        &self,
        name: &str,
        etag: Option<&str>,
    ) -> Result<Fetched, RegistryError> {
        let url = format!("{}/{}", self.base_url, Self::encode_name(name));

        let Some((body, etag)) = self.get_packument(&url, etag, || {
            RegistryError::PackageNotFound(name.to_string())
        })?
        else {
            return Ok(Fetched::NotModified);
        };

        let packument =
            serde_json::from_str(&body).map_err(|source| RegistryError::MalformedResponse {
                url,
                source: Box::new(source),
            })?;
        Ok(Fetched::Body {
            packument: Box::new(packument),
            etag,
        })
    }

    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>, RegistryError> {
        Self::with_retries(|| match self.agent.get(url).call() {
            Ok(response) if response.status() == 404 => {
                Err((RegistryError::PackageNotFound(url.to_string()), Retry::No))
            }
            Ok(response) if !response.status().is_success() => {
                Err(Self::from_status(url, &response))
            }
            Ok(mut response) => response
                .body_mut()
                .with_config()
                .limit(MAX_TARBALL_BYTES)
                .read_to_vec()
                .map_err(|source| {
                    (
                        RegistryError::Network {
                            url: url.to_string(),
                            source: Box::new(source),
                        },
                        Retry::Transient,
                    )
                }),
            Err(source) => Err((
                RegistryError::Network {
                    url: url.to_string(),
                    source: Box::new(source),
                },
                Retry::Transient,
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::ABC_SHA512_SSRI;

    #[test]
    fn dist_prefers_the_integrity_field() {
        let dist = Dist {
            tarball: "https://example.test/x.tgz".into(),
            integrity: Some(ABC_SHA512_SSRI.into()),
            shasum: Some("a9993e364706816aba3e25717850c26c9cd0d89d".into()),
        };
        assert_eq!(
            dist.integrity().unwrap().algo,
            crate::integrity::Algo::Sha512
        );
    }

    #[test]
    fn dist_falls_back_to_a_legacy_shasum() {
        // Packages published before roughly 2017 have no integrity field.
        let dist = Dist {
            tarball: "https://example.test/x.tgz".into(),
            integrity: None,
            shasum: Some("a9993e364706816aba3e25717850c26c9cd0d89d".into()),
        };
        assert_eq!(dist.integrity().unwrap().algo, crate::integrity::Algo::Sha1);
    }

    #[test]
    fn dist_reports_when_neither_is_present() {
        let dist = Dist {
            tarball: "https://example.test/x.tgz".into(),
            integrity: None,
            shasum: None,
        };
        assert!(matches!(
            dist.integrity(),
            Err(crate::integrity::IntegrityError::Missing)
        ));
    }

    #[test]
    fn packument_deserializes_a_registry_response() {
        let raw = r#"{
            "name": "lodash",
            "dist-tags": { "latest": "4.17.21", "next": "5.0.0-beta.1" },
            "versions": {
                "4.17.20": { "name": "lodash", "version": "4.17.20",
                    "dist": { "tarball": "https://r.test/a.tgz" } },
                "4.17.21": { "name": "lodash", "version": "4.17.21",
                    "dist": { "tarball": "https://r.test/b.tgz" } }
            }
        }"#;

        let p: Packument = serde_json::from_str(raw).unwrap();
        assert_eq!(p.name, "lodash");
        assert_eq!(p.versions.len(), 2);
        assert_eq!(p.resolve_tag("latest"), Some("4.17.21"));
        assert_eq!(p.resolve_tag("nope"), None);
    }

    #[test]
    fn dev_dependencies_are_dropped_at_deserialization() {
        // The abbreviated packument really does carry devDependencies — a live
        // express version has them — so this is not hypothetical. Having no
        // field for them is precisely the mechanism that drops them, and a
        // dependency's dev dependencies must never be followed: doing so pulls
        // in most of the registry.
        let raw = r#"{
            "name": "a", "version": "1.0.0",
            "dist": { "tarball": "https://r.test/a.tgz" },
            "dependencies": { "runtime-dep": "^1.0.0" },
            "devDependencies": { "test-only-dep": "^2.0.0" }
        }"#;

        let metadata: VersionMetadata = serde_json::from_str(raw).unwrap();

        assert_eq!(metadata.dependencies.len(), 1);
        assert_eq!(metadata.dependencies["runtime-dep"], "^1.0.0");
        assert!(
            !metadata.dependencies.contains_key("test-only-dep"),
            "a devDependency leaked into the runtime dependency map"
        );
    }

    #[test]
    fn packument_skips_versions_it_cannot_parse() {
        // The registry has accumulated some genuinely malformed versions over
        // the years. One bad entry must not make a package unresolvable.
        let raw = r#"{
            "name": "old",
            "dist-tags": {},
            "versions": {
                "1.0.0":     { "name": "old", "version": "1.0.0",
                    "dist": { "tarball": "https://r.test/a.tgz" } },
                "not-a-ver": { "name": "old", "version": "not-a-ver",
                    "dist": { "tarball": "https://r.test/b.tgz" } }
            }
        }"#;

        let p: Packument = serde_json::from_str(raw).unwrap();
        let sorted = p.versions_sorted();
        assert_eq!(
            sorted.len(),
            1,
            "the unparseable version is dropped, not fatal"
        );
        assert_eq!(sorted[0].as_str(), "1.0.0");
    }

    #[test]
    fn packument_versions_come_back_in_precedence_order() {
        // Registry key order is lexical, which puts 1.10.0 before 1.9.0.
        let raw = r#"{
            "name": "p", "dist-tags": {},
            "versions": {
                "1.9.0":  { "name": "p", "version": "1.9.0",  "dist": { "tarball": "https://r.test/a.tgz" } },
                "1.10.0": { "name": "p", "version": "1.10.0", "dist": { "tarball": "https://r.test/b.tgz" } }
            }
        }"#;

        let p: Packument = serde_json::from_str(raw).unwrap();
        let sorted = p.versions_sorted();
        let order: Vec<&str> = sorted.iter().map(|v| v.as_str()).collect();
        assert_eq!(order, ["1.9.0", "1.10.0"]);
    }

    #[test]
    fn version_metadata_deserializes_a_registry_response() {
        let raw = r#"{
            "name": "lodash",
            "version": "4.17.21",
            "description": "ignored",
            "dist": {
                "tarball": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                "shasum": "679591c564c3bffaae8454cf0b3df370c3d6911c",
                "integrity": "sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg=="
            }
        }"#;

        let metadata: VersionMetadata = serde_json::from_str(raw).unwrap();
        assert_eq!(metadata.name, "lodash");
        assert_eq!(metadata.version, "4.17.21");
        assert!(metadata.dist.integrity.is_some());
    }
}
