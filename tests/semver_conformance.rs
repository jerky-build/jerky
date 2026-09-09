//! Differential conformance against npm's own semver.
//!
//! The oracle is committed rather than generated, so this runs offline and CI
//! never depends on the registry being reachable. See
//! `docs/superpowers/research/2026-09-08-npm-semver-crate-selection.md` for how
//! it was produced and why this suite exists: `js-semver` is pre-1.0, and this
//! is what makes replacing it one test run rather than a repeat investigation.
//!
//! A failure here is never fixed by editing the fixture. It is ground truth
//! from the reference implementation; a divergence means the wrapper or the
//! crate is wrong.

use jerky::range::{Range, Version};
use serde::Deserialize;

#[derive(Deserialize)]
struct Oracle {
    oracle: String,
    /// The exact version set the cases were generated against, carried in the
    /// fixture so it cannot drift out of step with them.
    versions: Vec<String>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    range: String,
    valid: bool,
    satisfying: Vec<String>,
}

#[test]
fn matches_npm_semver_on_every_recorded_case() {
    let raw = include_str!("fixtures/semver-oracle.json");
    let oracle: Oracle = serde_json::from_str(raw).expect("fixture is valid JSON");

    assert!(
        oracle.cases.len() > 500,
        "oracle looks truncated: {} cases",
        oracle.cases.len()
    );
    assert!(
        oracle.cases.iter().any(|c| !c.valid),
        "oracle contains no invalid ranges, so it cannot catch a permissive parser"
    );

    let versions: Vec<Version> = oracle
        .versions
        .iter()
        .map(|v| Version::parse(v).expect("oracle version parses"))
        .collect();

    let mut failures = Vec::new();

    for case in &oracle.cases {
        match (Range::parse(&case.range), case.valid) {
            (Err(_), false) => {}
            (Err(_), true) => {
                failures.push(format!("{:?}: refused a range npm accepts", case.range))
            }
            (Ok(_), false) => {
                failures.push(format!("{:?}: accepted a range npm rejects", case.range))
            }
            (Ok(range), true) => {
                let got: Vec<&str> = versions
                    .iter()
                    .filter(|v| range.matches(v))
                    .map(|v| v.as_str())
                    .collect();
                let want: Vec<&str> = case.satisfying.iter().map(String::as_str).collect();
                if got != want {
                    failures.push(format!(
                        "{:?}: got {got:?}, {} says {want:?}",
                        case.range, oracle.oracle
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} cases diverge from {}:\n{}",
        failures.len(),
        oracle.cases.len(),
        oracle.oracle,
        failures.join("\n")
    );
}
