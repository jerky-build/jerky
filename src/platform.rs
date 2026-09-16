//! Which machines a package says it runs on, and which machine this is.
//!
//! Two types that are easy to confuse and must not be: [`Platform`] is the
//! machine, [`PlatformSupport`] is what a package *declared* about the
//! machines it is for. Only the second is published, and only the first is
//! discovered, so keeping them apart is what stops a comparison being written
//! the wrong way round.
//!
//! This exists for one reason: `optionalDependencies` in real trees is
//! overwhelmingly a list of platform-specific binaries — `@esbuild/*`,
//! `@rollup/rollup-*`, `@napi-rs/*`, `fsevents` — each declaring the single
//! `os`/`cpu` pair it was built for, and an installer is expected to take the
//! one that fits. See `docs/specs/2026-09-16-optional-dependencies-design.md`.

/// The machine an install is happening on, in the registry's spelling.
///
/// Node's spelling, strictly: `os` and `cpu` are published against
/// `process.platform` and `process.arch`, so `darwin` and `x64` rather than
/// Rust's `macos` and `x86_64`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    os: String,
    cpu: String,
}

impl Platform {
    /// The machine this process is running on.
    ///
    /// Reads `std::env::consts`, which is a compile-time constant rather than
    /// an environment variable — so this is not the cwd-and-`$HOME` rule the
    /// invariants reserve to `main.rs`, and it is not a `#[cfg]` either. One
    /// definition of the mapping, every arm of it compiled everywhere.
    pub fn current() -> Self {
        Platform::new(
            node_os(std::env::consts::OS),
            node_cpu(std::env::consts::ARCH),
        )
    }

    /// A named machine.
    ///
    /// Private: the only machine anything outside this module has a question
    /// about is the one it is running on, and the tests that need to ask about
    /// another are in here. A public constructor would invite a caller to
    /// decide what platform an install is for, which is not a decision jerky
    /// offers.
    fn new(os: impl Into<String>, cpu: impl Into<String>) -> Self {
        Platform {
            os: os.into(),
            cpu: cpu.into(),
        }
    }
}

impl std::fmt::Display for Platform {
    /// `linux-x64`, which is how the ecosystem names a platform in a package
    /// name and therefore how a user will recognise one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.os, self.cpu)
    }
}

/// Rust's platform name in Node's spelling.
///
/// Only the names that both differ *and* can occur on a platform jerky
/// targets. `linux` is already the same word on both sides, so WSL and Linux
/// need no arm at all and `macos` is the whole of the list. An arm for
/// `windows` or `solaris` would be a second definition to keep in agreement
/// with the first, for a platform nothing runs on — the same thing the
/// `#[cfg(windows)]` invariant refuses, spelled as a match arm.
///
/// Anything unlisted passes through, which is the only honest answer for a
/// name neither side has heard of: the value is compared for equality and
/// nothing else, so it matches a package that names it and matches nothing
/// else.
fn node_os(rust: &str) -> &str {
    match rust {
        "macos" => "darwin",
        other => other,
    }
}

/// Rust's architecture name in Node's spelling. Same rule as [`node_os`],
/// applied to the architectures those platforms actually ship on.
fn node_cpu(rust: &str) -> &str {
    match rust {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/// What a package published about the machines it runs on: its `os` and `cpu`
/// lists, verbatim.
///
/// Verbatim because it is recorded in the lockfile and the lockfile is
/// platform independent — the file says what the package declared, and every
/// machine reading it reaches its own conclusion. Normalising here would bake
/// one machine's reading into a committed file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlatformSupport {
    pub os: Vec<String>,
    pub cpu: Vec<String>,
}

impl PlatformSupport {
    /// Will this package run here?
    ///
    /// Both lists must admit, and each does so under npm's rule — see
    /// [`admits_value`]. A package declaring neither admits everything, which
    /// is what nearly every package on the registry does.
    pub fn admits(&self, platform: &Platform) -> bool {
        admits_value(&self.os, &platform.os) && admits_value(&self.cpu, &platform.cpu)
    }
}

/// npm's `os`/`cpu` list rule, from `npm-install-checks`.
///
/// Reimplemented rather than paraphrased, because this is another place where
/// the correct answer is defined by another implementation's behaviour rather
/// than by a written specification. The rule:
///
/// - An entry may be negated with a leading `!`.
/// - A negated entry that matches rejects outright, whatever else is present.
/// - Otherwise the list admits if some non-negated entry matches, **or** if
///   every entry was negated.
/// - The single entry `any` admits anything, which npm special-cases and so
///   does this.
///
/// The last clause of the third rule is what makes an empty list admit, and it
/// is the case that matters: a package declaring no `os` at all arrives here
/// as one.
///
/// The mixed case is the one worth being explicit about. `["!win32",
/// "darwin"]` admits darwin and **nothing else** — not "anything but win32" —
/// because the list becomes a whitelist the moment a positive entry appears.
fn admits_value(list: &[String], value: &str) -> bool {
    if list.len() == 1 && list[0] == "any" {
        return true;
    }

    let mut negated = 0;
    let mut matched = false;
    for entry in list {
        match entry.strip_prefix('!') {
            Some(forbidden) => {
                negated += 1;
                if forbidden == value {
                    return false;
                }
            }
            None => matched |= entry == value,
        }
    }

    matched || negated == list.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supports(os: &[&str], cpu: &[&str]) -> PlatformSupport {
        PlatformSupport {
            os: os.iter().map(|s| s.to_string()).collect(),
            cpu: cpu.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_package_that_declares_nothing_runs_everywhere() {
        let anywhere = supports(&[], &[]);
        assert!(anywhere.admits(&Platform::new("linux", "x64")));
        assert!(anywhere.admits(&Platform::new("darwin", "arm64")));
    }

    #[test]
    fn a_positive_list_admits_only_what_it_names() {
        let darwin_arm64 = supports(&["darwin"], &["arm64"]);
        assert!(darwin_arm64.admits(&Platform::new("darwin", "arm64")));
        assert!(!darwin_arm64.admits(&Platform::new("darwin", "x64")));
        assert!(!darwin_arm64.admits(&Platform::new("linux", "arm64")));
    }

    #[test]
    fn both_lists_have_to_admit() {
        // `fsevents`: an `os` and no `cpu` at all, so the cpu half admits
        // everything and the os half decides.
        let fsevents = supports(&["darwin"], &[]);
        assert!(fsevents.admits(&Platform::new("darwin", "x64")));
        assert!(fsevents.admits(&Platform::new("darwin", "arm64")));
        assert!(!fsevents.admits(&Platform::new("linux", "x64")));
    }

    #[test]
    fn an_all_negated_list_admits_everything_it_does_not_forbid() {
        let not_windows = supports(&["!win32"], &[]);
        assert!(not_windows.admits(&Platform::new("linux", "x64")));
        assert!(not_windows.admits(&Platform::new("darwin", "arm64")));
        assert!(!not_windows.admits(&Platform::new("win32", "x64")));
    }

    #[test]
    fn a_mixed_list_is_a_whitelist_not_an_exclusion() {
        // The case a paraphrase of npm's rule gets wrong: a positive entry
        // turns the list into a whitelist, so this admits darwin and nothing
        // else rather than "anything but win32".
        let mixed = supports(&["!win32", "darwin"], &[]);
        assert!(mixed.admits(&Platform::new("darwin", "x64")));
        assert!(!mixed.admits(&Platform::new("linux", "x64")));
        assert!(!mixed.admits(&Platform::new("win32", "x64")));
    }

    #[test]
    fn a_negated_entry_beats_a_positive_one_for_the_same_value() {
        let contradictory = supports(&["darwin", "!darwin"], &[]);
        assert!(!contradictory.admits(&Platform::new("darwin", "x64")));
    }

    #[test]
    fn the_any_marker_admits_anything() {
        let any = supports(&["any"], &["any"]);
        assert!(any.admits(&Platform::new("win32", "ia32")));
    }

    #[test]
    fn rust_platform_names_are_translated_into_nodes() {
        assert_eq!(node_os("macos"), "darwin");
        assert_eq!(node_os("linux"), "linux");
        assert_eq!(node_cpu("x86_64"), "x64");
        assert_eq!(node_cpu("aarch64"), "arm64");
        assert_eq!(
            node_cpu("s390x"),
            "s390x",
            "an unlisted name passes through rather than becoming nothing"
        );
    }

    #[test]
    fn the_current_platform_is_one_this_project_targets() {
        // jerky targets WSL, Linux and macOS, and the suite's fixtures lean on
        // that: a package declaring `os: ["win32"]` must be skipped wherever
        // the tests run, and one declaring `["linux", "darwin"]` must not.
        let here = Platform::current();
        assert!(supports(&["linux", "darwin"], &[]).admits(&here));
        assert!(!supports(&["win32"], &[]).admits(&here));
    }

    #[test]
    fn a_platform_reads_the_way_the_ecosystem_names_one() {
        assert_eq!(Platform::new("linux", "x64").to_string(), "linux-x64");
    }
}
