//! The resolver's tree-shape tests.
//!
//! Each one is a claim about the design rather than coverage for its own sake:
//! that a diamond dedupes, that a conflict deliberately does not, that a cycle
//! terminates, and that the walk is iterative.

use std::collections::BTreeMap;

use jerky::resolver::{ResolveError, resolve};
use jerky::testing::FixtureRegistry;

fn roots(list: &[(&str, &str)]) -> BTreeMap<String, String> {
    list.iter()
        .map(|(n, r)| (n.to_string(), r.to_string()))
        .collect()
}

#[test]
fn resolves_a_diamond_to_one_shared_node() {
    // a -> b, a -> c, b -> d, c -> d.  d must appear exactly once.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("d", "^1.0.0")]),
        ("c", "1.0.0", &[("d", "^1.0.0")]),
        ("d", "1.0.0", &[]),
    ]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    assert_eq!(graph.packages.len(), 4);
    let ds: Vec<_> = graph.packages.keys().filter(|id| id.name == "d").collect();
    assert_eq!(ds.len(), 1, "d was duplicated");
}

#[test]
fn incompatible_versions_coexist_as_separate_nodes() {
    // The case that is hard under hoisting and nearly free in a virtual store.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("d", "^1.0.0")]),
        ("c", "1.0.0", &[("d", "^2.0.0")]),
        ("d", "1.5.0", &[]),
        ("d", "2.1.0", &[]),
    ]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    let mut ds: Vec<_> = graph
        .packages
        .keys()
        .filter(|id| id.name == "d")
        .map(|id| id.version.as_str())
        .collect();
    ds.sort();
    assert_eq!(ds, ["1.5.0", "2.1.0"]);

    // And each dependent points at the one it asked for.
    let b = graph.packages.values().find(|p| p.id.name == "b").unwrap();
    let c = graph.packages.values().find(|p| p.id.name == "c").unwrap();
    assert_eq!(b.dependencies["d"].version, "1.5.0");
    assert_eq!(c.dependencies["d"].version, "2.1.0");
}

#[test]
fn a_cycle_terminates() {
    // a -> b -> a. Real packages do this.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0")]),
        ("b", "1.0.0", &[("a", "^1.0.0")]),
    ]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    assert_eq!(graph.packages.len(), 2);
    let a = graph.packages.values().find(|p| p.id.name == "a").unwrap();
    let b = graph.packages.values().find(|p| p.id.name == "b").unwrap();
    assert_eq!(a.dependencies["b"].version, "1.0.0");
    assert_eq!(
        b.dependencies["a"].version, "1.0.0",
        "the back edge is recorded"
    );
}

#[test]
fn a_self_referencing_package_terminates() {
    // Rarer than a two-node cycle but it exists, and it is the tighter case.
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[("a", "^1.0.0")])]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    assert_eq!(graph.packages.len(), 1);
}

#[test]
fn a_deep_chain_does_not_blow_the_stack() {
    // Which is why the walk is a worklist rather than recursion. The depth is
    // far past anything real — npm trees are tens deep, not thousands — and is
    // chosen so a recursive implementation would plausibly exhaust a test
    // thread's stack rather than merely being slow. It costs under half a
    // second, so there is no reason to test a depth that proves less.
    let mut registry = FixtureRegistry::new();
    for i in 0..5000 {
        let name = format!("p{i}");
        if i < 4999 {
            let next = format!("p{}", i + 1);
            registry = registry.with_packument(&name, &[("1.0.0", &[(next.as_str(), "^1.0.0")])]);
        } else {
            registry = registry.with_packument(&name, &[("1.0.0", &[])]);
        }
    }

    let graph = resolve(&registry, &roots(&[("p0", "^1.0.0")])).unwrap();

    assert_eq!(graph.packages.len(), 5000);
}

#[test]
fn the_packument_for_a_package_is_fetched_once() {
    // Counted, because the resolved graph is identical whether or not the
    // cache worked.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("shared", "^1.0.0")]),
        ("c", "1.0.0", &[("shared", "^1.0.0")]),
        ("shared", "1.0.0", &[]),
    ]);

    resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    assert_eq!(registry.packument_calls_for("shared"), 1);
}

#[test]
fn distinct_ranges_on_one_package_still_share_a_packument() {
    // The range cache keys on (name, range), so two different ranges are two
    // lookups — but the packument behind them must still be fetched once.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("d", "^1.0.0")]),
        ("c", "1.0.0", &[("d", "^2.0.0")]),
        ("d", "1.5.0", &[]),
        ("d", "2.1.0", &[]),
    ]);

    resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    assert_eq!(registry.packument_calls_for("d"), 1);
}

#[test]
fn an_unsatisfiable_range_names_the_available_versions() {
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);

    let err = resolve(&registry, &roots(&[("a", "^9.0.0")])).unwrap_err();

    match err {
        ResolveError::Unsatisfiable {
            name,
            range,
            available,
        } => {
            assert_eq!(name, "a");
            assert_eq!(range, "^9.0.0");
            assert!(
                available.contains(&"1.0.0".to_string()),
                "the error must say what does exist"
            );
        }
        other => panic!("wrong error: {other:?}"),
    }
}

#[test]
fn an_unknown_package_is_reported() {
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);

    assert!(matches!(
        resolve(&registry, &roots(&[("nope", "^1.0.0")])),
        Err(ResolveError::Registry(_))
    ));
}

#[test]
fn dist_tags_resolve_through_the_packument() {
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[]), ("a", "2.0.0", &[])]);

    let graph = resolve(&registry, &roots(&[("a", "latest")])).unwrap();

    assert_eq!(graph.packages.len(), 1);
    assert_eq!(graph.packages.keys().next().unwrap().version, "2.0.0");
}

#[test]
fn the_root_ranges_are_recorded_verbatim() {
    // What makes lockfile staleness detectable later.
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    assert_eq!(graph.root.get("a").map(String::as_str), Some("^1.0.0"));
}

#[test]
fn resolution_is_deterministic() {
    // Byte-identical lockfiles depend on this. Ordering must come from the
    // structure, not from whatever order the walk happened to take.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("d", "^1.0.0")]),
        ("c", "1.0.0", &[("d", "^1.0.0")]),
        ("d", "1.0.0", &[]),
    ]);

    let first = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    let second = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    let ids = |g: &jerky::resolver::ResolvedGraph| {
        g.packages
            .keys()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&first), ids(&second));
}
