//! npm version ranges, and the only module that names `js-semver`.
//!
//! The wrapper is the containment boundary for a pre-1.0 dependency: swapping
//! the crate is a change to this file alone, and
//! `tests/semver_conformance.rs` decides whether a replacement is acceptable.
//! See `docs/research/2026-09-08-npm-semver-crate-selection.md`.

use std::cmp::Ordering;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RangeError {
    #[error("could not parse version `{0}`")]
    BadVersion(String),
    #[error("could not parse range `{0}`")]
    Unparseable(String),
}

/// A concrete semantic version.
///
/// Carries the registry's original spelling alongside the parsed form. The
/// parsed form drives comparison; the string is what reaches `package.json`
/// and the lockfile, so a round trip never normalises what the registry said.
///
/// **Equality is semantic, not textual.** Build metadata is ignored in
/// comparison, per the semver spec, so `1.2.3+a == 1.2.3` while their
/// `as_str` differ. Two such versions are interchangeable for selection but
/// not for display, and which spelling `max_satisfying` returns among equals
/// is unspecified. This is theoretical against a real registry — a packument
/// cannot hold two version keys differing only by build metadata, because
/// they would be the same key — but it is the kind of thing worth knowing
/// before relying on the pair.
#[derive(Debug, Clone)]
pub struct Version {
    parsed: js_semver::Version,
    raw: String,
}

impl Version {
    pub fn parse(input: &str) -> Result<Self, RangeError> {
        js_semver::Version::parse(input)
            .map(|parsed| Version {
                parsed,
                raw: input.to_string(),
            })
            .map_err(|_| RangeError::BadVersion(input.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Is this a prerelease — `5.0.0-beta.1` rather than `5.0.0`?
    ///
    /// Asked of the parsed form rather than by looking for a `-`, which would
    /// also find one inside build metadata: `1.0.0+build-7` is not a
    /// prerelease.
    pub fn is_prerelease(&self) -> bool {
        !self.parsed.pre_release.is_empty()
    }
}

// Ordering and equality are semantic, delegating to the parsed form. Comparing
// `raw` would sort `1.10.0` below `1.9.0`, which is the whole reason versions
// are not strings.
impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.parsed == other.parsed
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parsed.cmp(&other.parsed)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

/// An npm version range: `^1.2.3`, `1.2.x`, `>=1 <2`, `1.2.3 - 2.0.0`, unions.
#[derive(Debug, Clone)]
pub struct Range(js_semver::Range);

impl Range {
    pub fn parse(input: &str) -> Result<Self, RangeError> {
        js_semver::Range::parse(input)
            .map(Range)
            .map_err(|_| RangeError::Unparseable(input.to_string()))
    }

    pub fn matches(&self, version: &Version) -> bool {
        self.0.satisfies(&version.parsed)
    }

    /// Does this range rule nothing out?
    ///
    /// `*` has several spellings — `x`, `X`, `*.*.*`, `>=0.0.0`, and any union
    /// containing one — so the question is asked of what the range *admits*
    /// rather than of how it was written. A blocklist of literals would catch
    /// whichever spellings its author thought of.
    ///
    /// The ceiling is not a sentinel chosen for being large. js-semver, like
    /// node-semver before it, refuses a version component above
    /// `MAX_SAFE_INTEGER`, so this is the highest version the domain can
    /// express and no range can place a bound above it — which is what stops
    /// a genuinely bounded `<9999999.0.0` being mistaken for unbounded.
    pub fn admits_everything(&self) -> bool {
        // 2^53 - 1: the largest integer a JavaScript number holds exactly, and
        // the limit js-semver enforces on every version component.
        const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

        let floor = Version::parse("0.0.0").expect("a literal version parses");
        let ceiling = Version::parse(&format!(
            "{MAX_SAFE_INTEGER}.{MAX_SAFE_INTEGER}.{MAX_SAFE_INTEGER}"
        ))
        .expect("the highest expressible version parses");

        self.matches(&floor) && self.matches(&ceiling)
    }

    /// The highest published version satisfying this range, matching npm's
    /// selection rule.
    ///
    /// `None` means the range is unsatisfiable against this list, which is the
    /// caller's cue to report what versions do exist — "no matching version"
    /// without the candidates is the kind of error that sends people to a
    /// browser.
    pub fn max_satisfying<'v>(&self, versions: &'v [Version]) -> Option<&'v Version> {
        versions.iter().filter(|v| self.matches(v)).max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(list: &[&str]) -> Vec<Version> {
        list.iter().map(|v| Version::parse(v).unwrap()).collect()
    }

    #[test]
    fn caret_allows_minor_and_patch_on_one_x() {
        let r = Range::parse("^1.2.3").unwrap();
        assert!(r.matches(&Version::parse("1.9.9").unwrap()));
        assert!(!r.matches(&Version::parse("2.0.0").unwrap()));
    }

    #[test]
    fn caret_on_zero_x_is_narrower() {
        // npm's rule: ^0.2.3 allows 0.2.x but not 0.3.0. This is one of the
        // places Cargo's crate disagrees, so it is worth pinning here rather
        // than only in the conformance suite.
        let r = Range::parse("^0.2.3").unwrap();
        assert!(r.matches(&Version::parse("0.2.4").unwrap()));
        assert!(!r.matches(&Version::parse("0.3.0").unwrap()));
    }

    #[test]
    fn max_satisfying_picks_the_highest_not_the_first() {
        let r = Range::parse("^1.0.0").unwrap();
        let vs = versions(&["1.0.0", "1.9.9", "1.2.3", "2.0.0"]);
        assert_eq!(r.max_satisfying(&vs).unwrap().as_str(), "1.9.9");
    }

    #[test]
    fn max_satisfying_is_none_when_nothing_matches() {
        let r = Range::parse("^3.0.0").unwrap();
        assert!(r.max_satisfying(&versions(&["1.0.0", "2.0.0"])).is_none());
    }

    #[test]
    fn prereleases_are_excluded_unless_the_range_asks() {
        let plain = Range::parse("^1.0.0").unwrap();
        assert!(!plain.matches(&Version::parse("1.2.3-beta.1").unwrap()));

        let asks = Range::parse("^1.2.3-beta.1").unwrap();
        assert!(asks.matches(&Version::parse("1.2.3-beta.2").unwrap()));
    }

    #[test]
    fn parses_the_syntax_cargo_refuses() {
        // Every one of these is rejected outright by Cargo's semver crate.
        for r in [
            "1.2.3 - 2.3.4",
            ">=1.2.3 <2.0.0",
            "1.2.3 || >=3.0.0",
            "1.2.x",
            "*",
        ] {
            assert!(Range::parse(r).is_ok(), "{r} should parse");
        }
    }

    #[test]
    fn rejects_nonsense() {
        assert!(matches!(
            Range::parse("not a range"),
            Err(RangeError::Unparseable(_))
        ));
    }

    #[test]
    fn admits_everything_finds_every_spelling_of_the_wildcard() {
        for r in ["*", "x", "X", "*.*.*", "x.x.x", ">=0.0.0", "1.0.0 || *"] {
            assert!(
                Range::parse(r).unwrap().admits_everything(),
                "{r} rules nothing out"
            );
        }
    }

    #[test]
    fn admits_everything_is_not_fooled_by_a_merely_large_bound() {
        // The reason the ceiling is the highest *expressible* version rather
        // than a large-looking one: `<9999999.0.0` excludes something, and a
        // smaller probe would have reported it unbounded.
        for r in [
            "<9999999.0.0",
            "^0.0.0",
            "0.x",
            ">=1.0.0",
            "^4.0.0",
            "~4.17.0",
            ">=4 <5",
        ] {
            assert!(
                !Range::parse(r).unwrap().admits_everything(),
                "{r} rules something out"
            );
        }
    }

    #[test]
    fn is_prerelease_ignores_build_metadata() {
        // A `-` inside build metadata is not a prerelease marker, which is
        // why this asks the parsed form rather than searching the string.
        assert!(Version::parse("5.0.0-beta.1").unwrap().is_prerelease());
        assert!(!Version::parse("5.0.0").unwrap().is_prerelease());
        assert!(!Version::parse("1.0.0+build-7").unwrap().is_prerelease());
        assert!(
            Version::parse("1.0.0-rc.1+build-7")
                .unwrap()
                .is_prerelease()
        );
    }

    #[test]
    fn versions_order_by_precedence_with_prereleases_lower() {
        let mut vs = versions(&["1.0.0", "1.0.0-alpha", "0.9.9"]);
        vs.sort();
        let ordered: Vec<_> = vs.iter().map(|v| v.as_str()).collect();
        assert_eq!(ordered, ["0.9.9", "1.0.0-alpha", "1.0.0"]);
    }

    #[test]
    fn as_str_preserves_the_registry_spelling() {
        // The manifest and lockfile record what the registry said, so a
        // round trip through this type must not normalise it.
        assert_eq!(Version::parse("1.2.3").unwrap().as_str(), "1.2.3");
        assert_eq!(
            Version::parse("1.2.3-beta.2").unwrap().as_str(),
            "1.2.3-beta.2"
        );
    }

    #[test]
    fn rejects_an_unparseable_version() {
        assert!(matches!(
            Version::parse("not-a-version"),
            Err(RangeError::BadVersion(_))
        ));
    }
}
