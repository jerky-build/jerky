//! The peer pass's tree-shape tests.
//!
//! Each one is a claim about the design: that a peer is satisfied from the
//! nearest provider and not merely any, that disagreeing consumers get
//! distinct nodes, that the duplication reaches the ancestors that caused it,
//! and that a peer cycle terminates.

use std::collections::BTreeMap;

use jerky::resolver::{
    Declared, ImporterPath, Kind, Resolution, ResolvedGraph, resolve, resolve_peers,
};
use jerky::testing::FixtureRegistry;

fn section(list: &[(&str, &str)], kind: Kind) -> BTreeMap<String, Declared> {
    list.iter()
        .map(|(name, specifier)| {
            (
                name.to_string(),
                Declared {
                    specifier: specifier.to_string(),
                    kind,
                },
            )
        })
        .collect()
}

fn roots(list: &[(&str, &str)]) -> BTreeMap<ImporterPath, BTreeMap<String, Declared>> {
    BTreeMap::from([(ImporterPath::root(), section(list, Kind::Prod))])
}

fn no_members() -> BTreeMap<String, ImporterPath> {
    BTreeMap::new()
}

/// Every edge names a node the graph holds.
///
/// Worth asserting rather than assuming: the first cut of the pass broke peer
/// cycles by handing the back-edge an id it never went on to insert, and every
/// test still passed because none of them looked.
fn assert_no_dangling_edges(graph: &ResolvedGraph) {
    for package in graph.packages.values() {
        for (name, target) in &package.dependencies {
            assert!(
                graph.packages.contains_key(target),
                "{} depends on {name} -> {target}, which the graph does not hold",
                package.id
            );
        }
    }

    for package in graph.packages.values() {
        for (name, target) in &package.peers {
            assert!(
                graph.packages.contains_key(target),
                "{} resolved peer {name} -> {target}, which the graph does not hold",
                package.id
            );
        }
    }

    for (path, importer) in &graph.importers {
        for (name, dependency) in &importer.dependencies {
            if let Resolution::Registry(id) = &dependency.resolution {
                assert!(
                    graph.packages.contains_key(id),
                    "importer {path} links {name} -> {id}, which the graph does not hold"
                );
            }
        }
    }
}

/// Every key in the graph, rendered, so a test can state the tree it expects.
fn keys(graph: &ResolvedGraph) -> Vec<String> {
    graph.packages.keys().map(|id| id.to_string()).collect()
}

/// Every node of this package, rendered — one entry per copy the duplication
/// produced.
fn keys_named(graph: &ResolvedGraph, name: &str) -> Vec<String> {
    graph
        .packages
        .keys()
        .filter(|id| id.name == name)
        .map(|id| id.to_string())
        .collect()
}

#[test]
fn a_peer_is_satisfied_by_the_importers_own_dependency() {
    // The base case: the consumer is the project itself.
    let registry = FixtureRegistry::new()
        .with_tree(&[("react-dom", "18.2.0", &[]), ("react", "18.2.0", &[])])
        .with_declared_peers("react-dom", "18.2.0", &[("react", "^18.0.0", false)]);

    let graph = resolve(
        &registry,
        &roots(&[("react-dom", "^18.0.0"), ("react", "^18.0.0")]),
        &no_members(),
    )
    .unwrap();
    let (graph, unsatisfied) = resolve_peers(graph);

    assert!(unsatisfied.is_empty(), "{unsatisfied:?}");
    assert_no_dangling_edges(&graph);
    assert_eq!(
        keys_named(&graph, "react-dom"),
        ["react-dom@18.2.0(react@18.2.0)"]
    );
}

#[test]
fn a_peer_takes_the_nearest_provider_not_merely_any() {
    // Both the root and `mid` provide `react`, at different versions. The rule
    // is *nearest ancestor*, so `mid`'s must win — a test that only had one
    // provider would pass under "any ancestor" too, and prove nothing.
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("mid", "1.0.0", &[("leaf", "^1.0.0"), ("react", "17.0.0")]),
            ("leaf", "1.0.0", &[]),
            ("react", "17.0.0", &[]),
            ("react", "18.2.0", &[]),
        ])
        .with_declared_peers("leaf", "1.0.0", &[("react", ">=17", false)]);

    let graph = resolve(
        &registry,
        &roots(&[("mid", "^1.0.0"), ("react", "18.2.0")]),
        &no_members(),
    )
    .unwrap();
    let (graph, unsatisfied) = resolve_peers(graph);

    assert!(unsatisfied.is_empty(), "{unsatisfied:?}");
    assert_eq!(
        keys_named(&graph, "leaf"),
        ["leaf@1.0.0(react@17.0.0)"],
        "the root's react@18.2.0 is further away than mid's react@17.0.0"
    );
}

#[test]
fn a_packages_own_dependency_satisfies_its_own_peer() {
    // The "I will take yours, but I ship a fallback" pattern: the same name in
    // both `dependencies` and `peerDependencies`. Its own copy wins over an
    // ancestor that also provides one.
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("tool", "1.0.0", &[("react", "17.0.0")]),
            ("react", "17.0.0", &[]),
            ("react", "18.2.0", &[]),
        ])
        .with_declared_peers("tool", "1.0.0", &[("react", ">=17", false)]);

    let graph = resolve(
        &registry,
        &roots(&[("tool", "^1.0.0"), ("react", "18.2.0")]),
        &no_members(),
    )
    .unwrap();
    let (graph, unsatisfied) = resolve_peers(graph);

    assert!(unsatisfied.is_empty(), "{unsatisfied:?}");
    assert_eq!(keys_named(&graph, "tool"), ["tool@1.0.0(react@17.0.0)"]);
}

#[test]
fn two_importers_disagreeing_give_one_package_two_nodes() {
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("plugin", "1.0.0", &[]),
            ("react", "17.0.0", &[]),
            ("react", "18.2.0", &[]),
        ])
        .with_declared_peers("plugin", "1.0.0", &[("react", ">=17", false)]);

    let roots = BTreeMap::from([
        (
            ImporterPath::new("apps/old").unwrap(),
            section(&[("plugin", "^1.0.0"), ("react", "17.0.0")], Kind::Prod),
        ),
        (
            ImporterPath::new("apps/new").unwrap(),
            section(&[("plugin", "^1.0.0"), ("react", "18.2.0")], Kind::Prod),
        ),
    ]);

    let graph = resolve(&registry, &roots, &no_members()).unwrap();
    let (graph, unsatisfied) = resolve_peers(graph);

    assert!(unsatisfied.is_empty(), "{unsatisfied:?}");
    assert_no_dangling_edges(&graph);
    assert_eq!(
        keys_named(&graph, "plugin"),
        ["plugin@1.0.0(react@17.0.0)", "plugin@1.0.0(react@18.2.0)"],
        "one published version, two peer resolutions, two nodes"
    );
}

#[test]
fn duplication_reaches_an_intermediate_that_declares_no_peers_itself() {
    // `wrapper` has no peers of its own, but must still become two nodes: it
    // points at a different `plugin` in each subtree, and if both copies keyed
    // the same the graph would silently keep one and drop the other's subtree.
    let registry = FixtureRegistry::new()
        .with_tree(&[
            (
                "app-x",
                "1.0.0",
                &[("wrapper", "^1.0.0"), ("react", "18.2.0")],
            ),
            (
                "app-y",
                "1.0.0",
                &[("wrapper", "^1.0.0"), ("react", "17.0.0")],
            ),
            ("wrapper", "1.0.0", &[("plugin", "^1.0.0")]),
            ("plugin", "1.0.0", &[]),
            ("react", "17.0.0", &[]),
            ("react", "18.2.0", &[]),
        ])
        .with_declared_peers("plugin", "1.0.0", &[("react", ">=17", false)]);

    let graph = resolve(
        &registry,
        &roots(&[("app-x", "^1.0.0"), ("app-y", "^1.0.0")]),
        &no_members(),
    )
    .unwrap();
    let (graph, unsatisfied) = resolve_peers(graph);

    assert!(unsatisfied.is_empty(), "{unsatisfied:?}");
    assert_no_dangling_edges(&graph);
    assert_eq!(
        keys_named(&graph, "wrapper"),
        [
            "wrapper@1.0.0(plugin@1.0.0(react@17.0.0))",
            "wrapper@1.0.0(plugin@1.0.0(react@18.2.0))",
        ],
        "a peer-less intermediate is duplicated by the subtree beneath it"
    );
    assert_eq!(
        keys_named(&graph, "plugin").len(),
        2,
        "and the plugin it wraps is duplicated too"
    );
}

#[test]
fn a_peer_cycle_terminates_without_dangling_edges() {
    // `a` peers `b` and `b` peers `a`, each also depending on the other. The
    // pass must not chase the loop forever — and, the part that is easy to get
    // wrong, the edge that closes the cycle must still name a node that exists.
    // A name is finite and owns its parts, so the cycle cannot be spelled out
    // inside one; what it must not do is invent an id nothing emits.
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("a", "1.0.0", &[("b", "^1.0.0")]),
            ("b", "1.0.0", &[("a", "^1.0.0")]),
        ])
        .with_declared_peers("a", "1.0.0", &[("b", "^1.0.0", false)])
        .with_declared_peers("b", "1.0.0", &[("a", "^1.0.0", false)]);

    let graph = resolve(
        &registry,
        &roots(&[("a", "^1.0.0"), ("b", "^1.0.0")]),
        &no_members(),
    )
    .unwrap();
    let (graph, unsatisfied) = resolve_peers(graph);

    assert!(unsatisfied.is_empty(), "{unsatisfied:?}");
    assert_no_dangling_edges(&graph);
    assert_eq!(
        keys_named(&graph, "a").len(),
        1,
        "one `a` reached, however many times the loop was entered"
    );
}

#[test]
fn an_unsatisfied_optional_peer_is_silent() {
    let registry = FixtureRegistry::new()
        .with_tree(&[("vite", "5.0.0", &[])])
        .with_declared_peers("vite", "5.0.0", &[("terser", "^5.4.0", true)]);

    let graph = resolve(&registry, &roots(&[("vite", "^5.0.0")]), &no_members()).unwrap();
    let (graph, unsatisfied) = resolve_peers(graph);

    assert!(
        unsatisfied.is_empty(),
        "an optional peer with no provider says nothing: {unsatisfied:?}"
    );
    assert_eq!(
        keys_named(&graph, "vite"),
        ["vite@5.0.0"],
        "and leaves the node undisturbed"
    );
}

#[test]
fn a_missing_required_peer_is_distinct_from_one_out_of_range() {
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("needs-missing", "1.0.0", &[]),
            ("needs-newer", "1.0.0", &[]),
            ("react", "17.0.0", &[]),
        ])
        .with_declared_peers("needs-missing", "1.0.0", &[("vue", "^3.0.0", false)])
        .with_declared_peers("needs-newer", "1.0.0", &[("react", "^18.0.0", false)]);

    let graph = resolve(
        &registry,
        &roots(&[
            ("needs-missing", "^1.0.0"),
            ("needs-newer", "^1.0.0"),
            ("react", "17.0.0"),
        ]),
        &no_members(),
    )
    .unwrap();
    let (_, unsatisfied) = resolve_peers(graph);

    let missing = unsatisfied
        .iter()
        .find(|u| u.peer == "vue")
        .expect("the missing peer is reported");
    assert_eq!(missing.found, None, "nothing provides vue at all");

    let out_of_range = unsatisfied
        .iter()
        .find(|u| u.peer == "react")
        .expect("the out-of-range peer is reported");
    assert_eq!(
        out_of_range.found.as_deref(),
        Some("17.0.0"),
        "a provider exists, and naming its version is what makes the two cases different"
    );
}

#[test]
fn a_tree_with_no_peers_comes_out_unchanged() {
    // The overwhelmingly common case. The pass must be a no-op on it, or every
    // existing lockfile would churn the moment peers landed.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("d", "^1.0.0")]),
        ("c", "1.0.0", &[("d", "^1.0.0")]),
        ("d", "1.0.0", &[]),
    ]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
    let before = keys(&graph);
    let (graph, unsatisfied) = resolve_peers(graph);

    assert!(unsatisfied.is_empty());
    assert_eq!(keys(&graph), before, "no peers, no change");
    assert_eq!(before, ["a@1.0.0", "b@1.0.0", "c@1.0.0", "d@1.0.0"]);
    assert_no_dangling_edges(&graph);
    let a = graph
        .packages
        .values()
        .find(|package| package.id.name == "a")
        .expect("a is in the graph");
    assert_eq!(
        a.dependencies
            .values()
            .map(|id| id.to_string())
            .collect::<Vec<_>>(),
        ["b@1.0.0", "c@1.0.0"],
        "edges are preserved, not only keys"
    );
}

#[test]
fn an_importer_points_at_the_duplicated_node() {
    // The rewrite has to reach the importer's own edges, not only the package
    // map — otherwise `node_modules` links at a key the graph no longer holds.
    let registry = FixtureRegistry::new()
        .with_tree(&[("plugin", "1.0.0", &[]), ("react", "18.2.0", &[])])
        .with_declared_peers("plugin", "1.0.0", &[("react", "^18.0.0", false)]);

    let graph = resolve(
        &registry,
        &roots(&[("plugin", "^1.0.0"), ("react", "^18.0.0")]),
        &no_members(),
    )
    .unwrap();
    let (graph, _) = resolve_peers(graph);

    let root = &graph.importers[&ImporterPath::root()];
    let Resolution::Registry(id) = &root.dependencies["plugin"].resolution else {
        panic!("plugin resolved to a registry package");
    };
    assert_eq!(id.to_string(), "plugin@1.0.0(react@18.2.0)");
    assert!(
        graph.packages.contains_key(id),
        "and the graph holds the node the importer names"
    );
}

#[test]
fn a_resolved_peer_names_the_node_the_graph_holds() {
    // `lib` peers on `host`, and `host` depends on `lib` — so naming `lib`
    // gives `host` a context of its own, and the plain `host@1.0.0` the peer
    // was found under stops being a node at all.
    //
    // A peer answer is therefore not the id that answered it; it is whatever
    // that provider was finally named. Recording the one the walk found leaves
    // `peers` pointing into a graph that no longer holds it — a link into a
    // directory the store never wrote, once the linker reads this — and it is
    // invisible to a test that only looks at keys, because the keys are right.
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("host", "1.0.0", &[("lib", "^1.0.0")]),
            ("lib", "1.0.0", &[]),
        ])
        .with_declared_peers("lib", "1.0.0", &[("host", "^1.0.0", false)]);

    let graph = resolve(&registry, &roots(&[("host", "^1.0.0")]), &no_members()).unwrap();
    let (graph, unsatisfied) = resolve_peers(graph);

    assert!(unsatisfied.is_empty(), "{unsatisfied:?}");
    assert_no_dangling_edges(&graph);
    assert_eq!(
        keys(&graph),
        ["host@1.0.0(lib@1.0.0(host@1.0.0))", "lib@1.0.0(host@1.0.0)"]
    );

    let lib = graph
        .packages
        .values()
        .find(|package| package.id.name == "lib")
        .expect("lib is in the graph");
    assert_eq!(
        lib.peers["host"].to_string(),
        "host@1.0.0(lib@1.0.0(host@1.0.0))"
    );
}
