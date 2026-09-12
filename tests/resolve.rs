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
fn the_root_importer_records_what_was_asked_and_what_was_chosen() {
    // The specifier is what makes staleness detectable; the resolution is what
    // saves linking from re-deriving which version a range picked.
    use jerky::resolver::{ImporterPath, Resolution};

    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    let root = &graph.importers[&ImporterPath::root()];
    let dependency = &root.dependencies["a"];
    assert_eq!(dependency.specifier, "^1.0.0");
    assert!(matches!(
        &dependency.resolution,
        Resolution::Registry(id) if id.version == "1.0.0"
    ));
}

#[test]
fn a_single_project_repo_is_a_workspace_of_one() {
    // No special case: the common shape is the degenerate case of the general
    // one, keyed `.`.
    use jerky::resolver::ImporterPath;

    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    assert_eq!(graph.importers.len(), 1);
    assert!(graph.importers.contains_key(&ImporterPath::root()));
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

/// A registry that serves a packument built by hand, so tests can express
/// shapes a well-behaved fixture cannot: a dist-tag pointing at nothing, or a
/// tag whose name is itself valid range syntax.
struct HandBuilt {
    versions: Vec<&'static str>,
    tags: Vec<(&'static str, &'static str)>,
}

impl jerky::registry::RegistryClient for HandBuilt {
    fn version_metadata(
        &self,
        _: &str,
        _: &str,
    ) -> Result<jerky::registry::VersionMetadata, jerky::registry::RegistryError> {
        unreachable!("the resolver only fetches packuments")
    }

    fn packument(
        &self,
        name: &str,
    ) -> Result<jerky::registry::Packument, jerky::registry::RegistryError> {
        use jerky::registry::{Dist, Packument, VersionMetadata};

        let versions = self
            .versions
            .iter()
            .map(|v| {
                (
                    v.to_string(),
                    VersionMetadata {
                        name: name.to_string(),
                        version: v.to_string(),
                        dist: Dist {
                            tarball: format!("https://hand.test/{name}-{v}.tgz"),
                            // sha512 of the empty string; only needs to parse.
                            integrity: Some(
                                "sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUdBeoGlODJ6+SfaPg==".into(),
                            ),
                            shasum: None,
                        },
                        dependencies: BTreeMap::new(),
                    },
                )
            })
            .collect();

        Ok(Packument {
            name: name.to_string(),
            versions,
            dist_tags: self
                .tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })
    }

    fn fetch_tarball(&self, _: &str) -> Result<Vec<u8>, jerky::registry::RegistryError> {
        unreachable!("the resolver never downloads")
    }
}

#[test]
fn a_dangling_dist_tag_is_an_error_not_a_panic() {
    // Unpublishing a version leaves its dist-tag behind, so this is a shape the
    // real registry produces. Trusting the tag's target would panic on lookup.
    let registry = HandBuilt {
        versions: vec!["1.0.0"],
        tags: vec![("latest", "9.9.9")],
    };

    let err = resolve(&registry, &roots(&[("a", "latest")])).unwrap_err();

    match err {
        ResolveError::DanglingTag { name, tag, version } => {
            assert_eq!(
                (name.as_str(), tag.as_str(), version.as_str()),
                ("a", "latest", "9.9.9")
            );
        }
        other => panic!("wrong error: {other:?}"),
    }
}

#[test]
fn a_dist_tag_cannot_shadow_a_real_range() {
    // Ranges are parsed before tags are consulted. Otherwise a registry could
    // publish a tag literally named `^1.0.0` and redefine what that range
    // selects — here, dragging a caret range up to a major it excludes.
    let registry = HandBuilt {
        versions: vec!["1.0.0", "1.5.0", "9.9.9"],
        tags: vec![("^1.0.0", "9.9.9")],
    };

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    let chosen = &graph.packages.keys().next().unwrap().version;
    assert_eq!(chosen, "1.5.0", "the tag overrode the range");
}

#[test]
fn a_spec_that_is_neither_a_range_nor_a_tag_is_reported() {
    let registry = HandBuilt {
        versions: vec!["1.0.0"],
        tags: vec![("latest", "1.0.0"), ("next", "1.0.0")],
    };

    let err = resolve(&registry, &roots(&[("a", "nonsense-spec")])).unwrap_err();

    match err {
        ResolveError::UnresolvableSpec { name, spec, tags } => {
            assert_eq!(name, "a");
            assert_eq!(spec, "nonsense-spec");
            // The error names what it could have been, rather than only what it wasn't.
            assert!(tags.contains(&"latest".to_string()));
            assert!(tags.contains(&"next".to_string()));
        }
        other => panic!("wrong error: {other:?}"),
    }
}
