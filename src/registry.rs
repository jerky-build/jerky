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
    // Separate from `Network` because the registry *was* reached: it took the
    // connection and then went quiet, which is a different next move from a
    // host that is down. #79 is about the two being indistinguishable to
    // whoever is watching a cursor, so this names the request that gave up and
    // leaves the cause underneath to name the deadline that ran out —
    // "timeout: receive response" for a registry that never answered,
    // "timeout: receive body" for one that stopped part-way through.
    #[error("the registry at {url} stopped responding, so jerky stopped waiting")]
    Stalled {
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
    /// Dependencies whose absence is tolerated, as `optionalDependencies`
    /// publishes them.
    ///
    /// A block of its own rather than folded into `dependencies`, because what
    /// makes one optional is the section it was declared in and nothing else —
    /// and because a name may appear in both, where npm's documented rule is
    /// that this one wins. Merging on the way in would lose the rule.
    ///
    /// Tolerated is narrower here than in npm: the only thing jerky skips is a
    /// package whose declared `os`/`cpu` rules this machine out. See
    /// `docs/specs/2026-09-16-optional-dependencies-design.md` §2.
    #[serde(default, rename = "optionalDependencies")]
    pub optional_dependencies: BTreeMap<String, String>,
    /// The platforms this package says it runs on: `process.platform` values,
    /// each optionally negated with `!`.
    ///
    /// Read verbatim and interpreted in `crate::platform`, because it is
    /// recorded in the lockfile and the lockfile is platform independent —
    /// every machine reading the file reaches its own conclusion from the same
    /// bytes.
    #[serde(default)]
    pub os: Vec<String>,
    /// The architectures this package says it runs on: `process.arch` values.
    /// Same rule as `os`, and both must admit.
    #[serde(default)]
    pub cpu: Vec<String>,
    /// What this package requires of its consumer's environment.
    ///
    /// Not an edge to resolve. `react-dom` declaring `react` here means
    /// "whoever installs me must give me a compatible react", and the answer
    /// depends on what that consumer already has — which is why this is read
    /// and then handed to a pass over the finished graph rather than walked.
    #[serde(default, rename = "peerDependencies")]
    pub peer_dependencies: BTreeMap<String, String>,
    /// Flags on the peers above. Only `optional` exists, and only its `true`
    /// case does anything: an unsatisfied optional peer is silent where a
    /// required one warns.
    ///
    /// An entry naming a peer that `peer_dependencies` does not declare is
    /// meaningless rather than malformed, and is simply never read.
    #[serde(default, rename = "peerDependenciesMeta")]
    pub peer_dependencies_meta: BTreeMap<String, PeerMeta>,
}

/// One peer's flags, as `peerDependenciesMeta` carries them.
///
/// There is deliberately no field for anything but `optional`. The key is
/// published as an object so it can grow, and everything it might grow is
/// something jerky would have to decide about before reading.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PeerMeta {
    #[serde(default)]
    pub optional: bool,
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

/// How long jerky waits at each phase of a request before calling it stalled.
///
/// A set of per-phase deadlines rather than one for the whole call, because
/// the phases are not the same question. Nothing up to the first byte of a
/// response scales with how big that response is: a DNS lookup, a handshake, a
/// request head and a registry's think time cost the same for a 200 KB
/// packument as for a 512 MB tarball, so a stall in any of them is a stall
/// whatever was asked for. Only the body is different — and a single deadline
/// generous enough for the largest body jerky will accept is no bound at all
/// on everything before it, which is why `timeout_global` is left unset.
///
/// ureq 3 has no *idle* deadline to offer, which is the one shape that would
/// suit a body: its socket read deadline is whatever remains of the phase's
/// allowance and is not restarted per read, so a body deadline is a total
/// budget or it is nothing. That is the whole reason the two paths carry
/// different numbers instead of sharing one.
struct Deadlines {
    /// Reaching the server: the name lookup, then the socket and any TLS
    /// handshake. One number for both, and ureq budgets them separately, so
    /// this is spent twice in the worst case.
    connect: Duration,
    /// Getting an answer out of it: the request head out, the response head
    /// back. One number for both — and so, likewise, spent twice — because
    /// neither scales with anything, and a peer that will not take a GET's
    /// headers is the same stall as one that takes them and then says nothing.
    response: Duration,
    /// Reading a metadata body, start to finish.
    metadata_body: Duration,
    /// Reading a tarball body, start to finish.
    tarball_body: Duration,
}

impl Deadlines {
    /// The numbers jerky ships.
    ///
    /// **Ten seconds to reach the registry**, spent once on the name lookup
    /// and again on the socket, since ureq budgets those separately. A lookup
    /// and a TLS handshake to a CDN either happen in about a second or are not
    /// going to; ten leaves room for a congested link without leaving room for
    /// a stall.
    ///
    /// **Thirty seconds for an answer**, likewise once for the request head
    /// going out and once for the response head coming back. The registry
    /// serves a packument in well under a second, so this is two orders of
    /// magnitude of headroom and still a bound. It is the one that ends #79,
    /// where a connection was accepted and then said nothing for eleven
    /// minutes.
    ///
    /// **A minute for a metadata body**, which is enormous for a document that
    /// is a few megabytes in its abbreviated form at the very largest.
    ///
    /// **Five minutes for a tarball body**, which is npm's own `fetch-timeout`
    /// default — except that npm spends it on the whole request and jerky
    /// spends it on the body alone, so jerky is strictly the more patient of
    /// the two with a slow download. That matters because a total budget
    /// cannot tell a stalled 512 MB download from a slow one, and killing a
    /// legitimate one would trade this bug for a worse bug; the number to lean
    /// on is the ecosystem's rather than a guess.
    ///
    /// A stall retries like any other transport failure, so what a user waits
    /// is three attempts and the backoff between them. In practice that is one
    /// phase's deadline three times over — a stalled request dies in the phase
    /// it stalled in and does not reach the next — and for a request that is
    /// answered directly, the upper bound is around seven minutes on a
    /// metadata request and nineteen on a tarball.
    ///
    /// A redirect multiplies the first of those. ureq restarts every phase
    /// budget on each hop and leaves ten hops available, so a registry that
    /// answers a tarball URL with a 302 to a CDN — which several do — can
    /// spend the pre-body phases up to eleven times over before the body
    /// starts. Still a bound, and still the thing this type exists to
    /// establish, but an hour rather than nineteen minutes in the worst case.
    /// `max_redirects` is the knob that shortens it, and it is left at ureq's
    /// default because refusing a hop a real registry needs would break an
    /// install to shorten a bound that only a hostile chain ever reaches.
    ///
    /// None of them are infinity, which is what they all were.
    const DEFAULT: Self = Self {
        connect: Duration::from_secs(10),
        response: Duration::from_secs(30),
        metadata_body: Duration::from_secs(60),
        tarball_body: Duration::from_secs(300),
    };

    /// Short deadlines for a test, in the production *shape*.
    ///
    /// Every phase but one takes `deadline`. The tarball body deliberately
    /// does not: it takes a multiple, because a test cannot prove the two body
    /// paths differ if the flattened version makes them the same. With one
    /// number everywhere, the agent's own body deadline fires at the instant
    /// the tarball path's raise would, so deleting the raise entirely leaves
    /// every test green — which is exactly the regression the raise exists to
    /// prevent.
    const fn short(deadline: Duration) -> Self {
        Self {
            connect: deadline,
            response: deadline,
            metadata_body: deadline,
            tarball_body: Duration::from_millis(deadline.as_millis() as u64 * 4),
        }
    }
}

/// The npm registry over HTTP.
pub struct HttpRegistry {
    base_url: String,
    agent: ureq::Agent,
    /// The agent carries the metadata body deadline, since that is what
    /// almost every request is; the tarball path raises it per request,
    /// because only that path knows it has asked for a body three orders of
    /// magnitude larger.
    tarball_body_deadline: Duration,
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
        Self::build(base_url, Deadlines::DEFAULT)
    }

    /// A client on short deadlines, scaled from `deadline`. See
    /// `Deadlines::short` for which phase is not simply `deadline` and why.
    ///
    /// Public because the tests that prove a stalled connection is given up on
    /// live in `tests/`, and a test that waited out `Deadlines::DEFAULT` would
    /// add half a minute to the suite to learn what a quarter of a second
    /// teaches. It is not configuration: nothing in the binary calls it and no
    /// flag reaches it, because how long to wait for a registry is jerky's
    /// answer to give rather than a knob to hand over.
    pub fn with_deadline(base_url: impl Into<String>, deadline: Duration) -> Self {
        Self::build(base_url, Deadlines::short(deadline))
    }

    fn build(base_url: impl Into<String>, deadlines: Deadlines) -> Self {
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
            // Bounding the name lookup costs a thread per *request*, not per
            // connection: ureq resolves before it asks the pool for one, so a
            // pooled connection does not skip the lookup, and the only way to
            // abandon a blocked `getaddrinfo` is to have run it somewhere that
            // can be abandoned. A lookup that does time out leaves its thread
            // parked until the system resolver gives up on it.
            //
            // Paid anyway. The threads are short-lived and never more than
            // `MAX_CONCURRENT_FETCHES` at a time, the alternative is trusting
            // limits that live in a file the user is free to have set to
            // something patient, and the phase is otherwise the one part of a
            // request with no bound on it at all.
            .timeout_resolve(Some(deadlines.connect))
            .timeout_connect(Some(deadlines.connect))
            .timeout_send_request(Some(deadlines.response))
            .timeout_recv_response(Some(deadlines.response))
            .timeout_recv_body(Some(deadlines.metadata_body))
            .build();

        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            agent: config.into(),
            tarball_body_deadline: deadlines.tarball_body,
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

    /// Turn a failure that never became a response into an error and a retry
    /// policy.
    ///
    /// A deadline that ran out is reported as a stall rather than as an
    /// unreachable registry, for the reason `Stalled` exists: the two are
    /// different situations and a reader acts on them differently.
    ///
    /// Both retry, on the transient schedule. A stall is a transport failure
    /// like a reset or a refused connection, and the only reason `with_retries`
    /// never saw one before is that it retries attempts that *return* — which
    /// a wait with no end never did. Bounding the wait is what puts a stalled
    /// connection back on the path that already existed for everything else
    /// that failed in transport.
    fn from_transport(url: &str, source: ureq::Error) -> (RegistryError, Retry) {
        let url = url.to_string();
        let err = if matches!(source, ureq::Error::Timeout(_)) {
            RegistryError::Stalled {
                url,
                source: Box::new(source),
            }
        } else {
            RegistryError::Network {
                url,
                source: Box::new(source),
            }
        };
        (err, Retry::Transient)
    }

    /// Turn a failure part-way through a metadata body into an error and a
    /// retry policy.
    ///
    /// Everything but a stall keeps the reading the size limits were written
    /// for: a body past `MAX_METADATA_BYTES` and a body that is not UTF-8 are
    /// both the registry sending something jerky cannot use. A stall is
    /// neither — the bytes that arrived were fine as far as they got — and
    /// reporting one as a malformed response would send the reader looking for
    /// a broken registry rather than a stuck one.
    fn from_metadata_body(url: &str, source: ureq::Error) -> (RegistryError, Retry) {
        if matches!(source, ureq::Error::Timeout(_)) {
            return Self::from_transport(url, source);
        }
        (
            RegistryError::MalformedResponse {
                url: url.to_string(),
                source: Box::new(source),
            },
            Retry::Transient,
        )
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
                    .map_err(|source| Self::from_metadata_body(url, source)),
                Err(source) => Err(Self::from_transport(url, source)),
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
                        .map_err(|source| Self::from_metadata_body(url, source))
                }
                Err(source) => Err(Self::from_transport(url, source)),
            }
        })
    }

    /// Does this package exist at all? Used only on the error path, to turn a
    /// 404 into either `PackageNotFound` or `VersionNotFound`.
    ///
    /// A probe that fails reports rather than answering. It used to collapse
    /// every error into "no", which turned an unreachable registry into a
    /// confident `PackageNotFound` for a package that was there all along —
    /// and once a stall is bounded rather than endless, that is the shape a
    /// stalled probe takes: jerky waits out the deadline and then says the
    /// package does not exist. Naming a package that is not missing is worse
    /// than admitting the question went unanswered.
    fn package_exists(&self, name: &str) -> Result<bool, RegistryError> {
        let url = format!("{}/{}", self.base_url, Self::encode_name(name));
        match self.agent.get(&url).call() {
            Ok(response) => Ok(response.status().is_success()),
            Err(source) => Err(Self::from_transport(&url, source).0),
        }
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
        let body = self.get_json(&url, false, || match self.package_exists(name) {
            Ok(true) => RegistryError::VersionNotFound {
                name: name.to_string(),
                version: version.to_string(),
            },
            Ok(false) => RegistryError::PackageNotFound(name.to_string()),
            // The probe could not say. Reporting why beats picking one of the
            // two answers it was asked to choose between.
            Err(err) => err,
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
        Self::with_retries(|| {
            // The one request that raises the agent's body deadline: a tarball
            // is the only body whose size is worth waiting minutes for, and
            // the budget is total rather than idle, so the metadata number
            // would cut off a large package on a slow link.
            let request = self
                .agent
                .get(url)
                .config()
                .timeout_recv_body(Some(self.tarball_body_deadline))
                .build();

            match request.call() {
                Ok(response) if response.status() == 404 => {
                    Err((RegistryError::PackageNotFound(url.to_string()), Retry::No))
                }
                Ok(response) if !response.status().is_success() => {
                    Err(Self::from_status(url, &response))
                }
                // Deliberately not `from_metadata_body`'s rule. A tarball's
                // bytes are not judged here — they are checked against the
                // integrity hash the packument published, downstream of this
                // — so there is no reading of them this layer could call
                // malformed. Everything that can fail here is transport, bar
                // a body past `MAX_TARBALL_BYTES`, which is rare enough and
                // loud enough not to be worth a second rule.
                Ok(mut response) => response
                    .body_mut()
                    .with_config()
                    .limit(MAX_TARBALL_BYTES)
                    .read_to_vec()
                    .map_err(|source| Self::from_transport(url, source)),
                Err(source) => Err(Self::from_transport(url, source)),
            }
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
    fn peer_dependencies_are_read_off_the_abbreviated_packument() {
        // Verified against the live registry: react-dom@18.2.0 returns
        // peerDependencies in the abbreviated form jerky already asks for, so
        // this needs no second request for a full packument.
        let raw = r#"{
            "name": "react-dom", "version": "18.2.0",
            "dist": { "tarball": "https://r.test/a.tgz" },
            "dependencies": { "scheduler": "^0.23.0" },
            "peerDependencies": { "react": "^18.2.0" }
        }"#;

        let metadata: VersionMetadata = serde_json::from_str(raw).unwrap();

        assert_eq!(metadata.peer_dependencies["react"], "^18.2.0");
        assert_eq!(
            metadata.dependencies["scheduler"], "^0.23.0",
            "a peer must not land in the runtime dependency map"
        );
    }

    #[test]
    fn peer_dependencies_meta_marks_a_peer_optional() {
        // vite's latest publishes seven optional peers this way.
        let raw = r#"{
            "name": "vite", "version": "5.0.0",
            "dist": { "tarball": "https://r.test/a.tgz" },
            "peerDependencies": { "terser": "^5.4.0", "sass": "^1.0.0" },
            "peerDependenciesMeta": { "terser": { "optional": true }, "sass": {} }
        }"#;

        let metadata: VersionMetadata = serde_json::from_str(raw).unwrap();

        assert!(metadata.peer_dependencies_meta["terser"].optional);
        assert!(
            !metadata.peer_dependencies_meta["sass"].optional,
            "an empty meta object leaves the peer required"
        );
    }

    #[test]
    fn a_version_with_no_peer_fields_parses_with_both_maps_empty() {
        // The overwhelmingly common case, and the one that must not regress.
        let raw = r#"{
            "name": "lodash", "version": "4.17.21",
            "dist": { "tarball": "https://r.test/a.tgz" }
        }"#;

        let metadata: VersionMetadata = serde_json::from_str(raw).unwrap();

        assert!(metadata.peer_dependencies.is_empty());
        assert!(metadata.peer_dependencies_meta.is_empty());
    }

    #[test]
    fn meta_naming_a_peer_that_is_not_declared_is_ignored_rather_than_fatal() {
        // Meaningless rather than malformed, and the registry has published
        // worse. Nothing downstream reads the entry, because the peer it names
        // is never walked.
        let raw = r#"{
            "name": "a", "version": "1.0.0",
            "dist": { "tarball": "https://r.test/a.tgz" },
            "peerDependencies": { "real": "^1.0.0" },
            "peerDependenciesMeta": { "ghost": { "optional": true } }
        }"#;

        let metadata: VersionMetadata = serde_json::from_str(raw).unwrap();

        assert_eq!(metadata.peer_dependencies.len(), 1);
        assert!(!metadata.peer_dependencies.contains_key("ghost"));
    }

    #[test]
    fn optional_dependencies_are_dropped_at_deserialization() {
        // They ride in the same abbreviated response as peers — vite's latest
        // carries both — and they are deliberately out of scope for peer
        // support. A field that exists is a field someone will read, so the
        // way to keep them unimplemented is to keep them unparsed.
        let raw = r#"{
            "name": "a", "version": "1.0.0",
            "dist": { "tarball": "https://r.test/a.tgz" },
            "dependencies": { "runtime-dep": "^1.0.0" },
            "optionalDependencies": { "fsevents": "^2.3.0" }
        }"#;

        let metadata: VersionMetadata = serde_json::from_str(raw).unwrap();

        assert_eq!(metadata.dependencies.len(), 1);
        assert!(
            !metadata.dependencies.contains_key("fsevents"),
            "an optionalDependency leaked into the runtime dependency map"
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
