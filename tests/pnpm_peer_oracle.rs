//! Differential parity against pnpm's own peer resolution.
//!
//! Peer resolution is the one part of jerky whose correct answer is defined by
//! another implementation's behaviour rather than by a written specification.
//! `tests/peers.rs` can therefore only prove jerky matches the spec document's
//! *reading* of the rule; this is what tests the reading. Real trees were
//! resolved once by pnpm and the answers committed, so the suite runs offline
//! and needs no pnpm on PATH — the pattern `tests/fixtures/semver-oracle.json`
//! already establishes.
//!
//! A failure here is not fixed by editing the fixture. It is a recording of
//! pnpm's behaviour; re-take it with
//! `tests/fixtures/generate-pnpm-peer-oracle.mjs` and read the diff.
//!
//! The comparison is over resolved structure, never over rendered keys. jerky
//! spells a peer context differently from pnpm on purpose, and a test that
//! compared spellings would report a deliberate difference as a failure.

use std::collections::{BTreeMap, BTreeSet};

use jerky::resolver::{Declared, ImporterPath, Kind, Resolution, resolve, resolve_peers};
use jerky::testing::FixtureRegistry;
use serde::Deserialize;

#[derive(Deserialize)]
struct Oracle {
    oracle: String,
    /// The pnpm settings the recording was taken under, carried in the fixture
    /// so the one that matters cannot drift out of it unnoticed.
    settings: Vec<String>,
    trees: Vec<Tree>,
}

#[derive(Deserialize, Clone)]
struct Tree {
    name: String,
    /// What this tree is in the set for. Checked against the recorded graph
    /// rather than believed, so a mislabelled tree fails instead of faking
    /// coverage.
    phenomenon: Phenomenon,
    importers: BTreeMap<String, BTreeMap<String, String>>,
    /// Workspace member package name -> the directory it sits in.
    members: BTreeMap<String, String>,
    packages: Vec<Published>,
    pnpm: Recorded,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "kebab-case")]
enum Phenomenon {
    ImporterSatisfied,
    AncestorSatisfied,
    DuplicatedByPeers,
    OptionalUnsatisfied,
}

/// One published version, as the registry serves it.
#[derive(Deserialize, Clone)]
struct Published {
    name: String,
    version: String,
    dependencies: BTreeMap<String, String>,
    peers: BTreeMap<String, PeerRange>,
}

#[derive(Deserialize, Clone)]
struct PeerRange {
    range: String,
    optional: bool,
}

/// pnpm's answer, with its key spelling discarded at generation: nodes are
/// numbered and every edge names a number.
#[derive(Deserialize, Clone)]
struct Recorded {
    nodes: Vec<Node>,
    importers: BTreeMap<String, BTreeMap<String, usize>>,
}

#[derive(Deserialize, Clone)]
struct Node {
    name: String,
    version: String,
    dependencies: BTreeMap<String, usize>,
    /// Only the peers that resolved. An unsatisfied one is an absent entry,
    /// because the thing under test is the edge that does not exist.
    peers: BTreeMap<String, usize>,
}

fn oracle() -> Oracle {
    let raw = include_str!("fixtures/pnpm-peer-oracle.json");
    serde_json::from_str(raw).expect("fixture is valid JSON")
}

/// A resolved graph reduced to what parity is about: who resolved against
/// whom. Both implementations are asked to produce one of these, which is what
/// lets a single comparison read them side by side.
struct Graph {
    nodes: Vec<Node>,
    importers: BTreeMap<String, BTreeMap<String, usize>>,
}

impl Graph {
    /// One node's whole resolved subtree, rendered.
    ///
    /// Recursive rather than a flat `name@version` plus its own peers: the
    /// duplication a peer causes propagates upward through packages that
    /// declare no peers at all, and a form that stopped at a node's own peers
    /// would see those copies as identical and collapse them.
    ///
    /// A peer can close a cycle — `browserslist` depends on
    /// `update-browserslist-db`, which peers back on `browserslist` — so a
    /// node already on the path renders as a back-reference and the recursion
    /// stops. That makes a rendering depend on which node it started from,
    /// which is harmless here because both sides start from the same places
    /// and render by the same rule.
    fn render(&self, at: usize, path: &mut Vec<usize>) -> String {
        let node = &self.nodes[at];
        if path.contains(&at) {
            return format!("^{}@{}", node.name, node.version);
        }

        path.push(at);
        let edges = |map: &BTreeMap<String, usize>, path: &mut Vec<usize>| {
            map.iter()
                .map(|(name, target)| format!("{name}={}", self.render(*target, path)))
                .collect::<Vec<_>>()
                .join(",")
        };
        let dependencies = edges(&node.dependencies, path);
        let peers = edges(&node.peers, path);
        path.pop();

        format!("{}@{} [{dependencies}] ({peers})", node.name, node.version)
    }

    /// Every node rendered, as a set.
    ///
    /// A set rather than a list because two nodes with identical subtrees are
    /// the same answer however many times an implementation chose to write it
    /// down: jerky keys an instance on everything its subtree can see, which
    /// is finer than it strictly needs to be, and the copies that come of that
    /// are indistinguishable once resolved.
    ///
    /// It is worth being clear about which way that blinds the comparison. An
    /// *extra* copy of a subtree already present goes unseen, which is the
    /// tolerance above. A *lost* one does not: two nodes collapsed into one —
    /// by a context hash colliding, spec §12's open question, or by a key that
    /// fails to tell them apart — removes a rendering pnpm still has, and that
    /// reads as "pnpm resolved a node jerky did not".
    fn answer(&self) -> (BTreeSet<String>, BTreeMap<String, BTreeMap<String, String>>) {
        let nodes = (0..self.nodes.len())
            .map(|at| self.render(at, &mut Vec::new()))
            .collect();

        let importers = self
            .importers
            .iter()
            .map(|(at, links)| {
                let links = links
                    .iter()
                    .map(|(name, target)| (name.clone(), self.render(*target, &mut Vec::new())))
                    .collect();
                (at.clone(), links)
            })
            .collect();

        (nodes, importers)
    }
}

fn importer(raw: &str) -> ImporterPath {
    ImporterPath::new(raw).expect("fixture importer path is workspace-relative")
}

/// Serve exactly the versions pnpm selected, at their published ranges.
///
/// Holding the candidate set to pnpm's answer is deliberate. Version selection
/// is the semver oracle's subject, and leaving it open here would let a
/// disagreement about *which version* surface as a disagreement about peers.
fn registry_for(packages: &[Published]) -> FixtureRegistry {
    let mut registry = FixtureRegistry::new();

    for package in packages {
        let dependencies: Vec<(&str, &str)> = package
            .dependencies
            .iter()
            .map(|(name, range)| (name.as_str(), range.as_str()))
            .collect();
        registry = registry.with_packument(
            &package.name,
            &[(package.version.as_str(), dependencies.as_slice())],
        );
    }

    // A second pass, because `with_declared_peers` amends a version that is
    // already registered and a peer can name a package listed later.
    for package in packages {
        if package.peers.is_empty() {
            continue;
        }
        let peers: Vec<(&str, &str, bool)> = package
            .peers
            .iter()
            .map(|(name, peer)| (name.as_str(), peer.range.as_str(), peer.optional))
            .collect();
        registry = registry.with_declared_peers(&package.name, &package.version, &peers);
    }

    registry
}

/// Resolve one tree the way jerky resolves any other, and reduce the result to
/// the comparable form.
fn jerky_resolves(tree: &Tree) -> Graph {
    let registry = registry_for(&tree.packages);

    let roots = tree
        .importers
        .iter()
        .map(|(at, declared)| {
            let section = declared
                .iter()
                .map(|(name, specifier)| {
                    (
                        name.clone(),
                        Declared {
                            specifier: specifier.clone(),
                            kind: Kind::Prod,
                        },
                    )
                })
                .collect();
            (importer(at), section)
        })
        .collect();

    let members = tree
        .members
        .iter()
        .map(|(name, at)| (name.clone(), importer(at)))
        .collect();

    let graph = resolve(&registry, &roots, &members)
        .unwrap_or_else(|error| panic!("{}: jerky could not resolve the tree: {error}", tree.name));
    let (graph, _) = resolve_peers(graph);

    let index: BTreeMap<_, _> = graph
        .packages
        .keys()
        .enumerate()
        .map(|(at, id)| (id.clone(), at))
        .collect();

    let nodes = graph
        .packages
        .values()
        .map(|package| Node {
            name: package.id.name.clone(),
            version: package.id.version.clone(),
            dependencies: package
                .dependencies
                .iter()
                .map(|(name, id)| (name.clone(), index[id]))
                .collect(),
            peers: package
                .peers
                .iter()
                .map(|(name, id)| (name.clone(), index[id]))
                .collect(),
        })
        .collect();

    let importers = graph
        .importers
        .iter()
        .map(|(at, importer)| {
            let links = importer
                .dependencies
                .iter()
                .filter_map(|(name, dependency)| match &dependency.resolution {
                    Resolution::Registry(id) => Some((name.clone(), index[id])),
                    Resolution::Local(_) => None,
                })
                .collect();
            (at.to_string(), links)
        })
        .collect();

    Graph { nodes, importers }
}

fn pnpm_resolved(tree: &Tree) -> Graph {
    Graph {
        nodes: tree.pnpm.nodes.clone(),
        importers: tree.pnpm.importers.clone(),
    }
}

/// Every way jerky's answer differs from pnpm's, in English.
///
/// Returned rather than asserted so the comparison itself can be put under
/// test: a parity check that passed on a deliberately wrong expectation would
/// be proving nothing, and there is no other way to find that out.
fn divergences(tree: &Tree) -> Vec<String> {
    let (mine, my_importers) = jerky_resolves(tree).answer();
    let (theirs, their_importers) = pnpm_resolved(tree).answer();

    let mut divergences = Vec::new();

    for node in mine.difference(&theirs) {
        divergences.push(format!("jerky resolved a node pnpm did not: {node}"));
    }
    for node in theirs.difference(&mine) {
        divergences.push(format!("pnpm resolved a node jerky did not: {node}"));
    }

    // Both directions, at both levels. An importer jerky invented, or a link
    // it added under one both sides have, is as much a disagreement as a
    // missing one — and walking only pnpm's side would see neither.
    let paths: BTreeSet<&String> = my_importers.keys().chain(their_importers.keys()).collect();
    for at in paths {
        match (my_importers.get(at), their_importers.get(at)) {
            (None, _) => {
                divergences.push(format!("pnpm has an importer `{at}` and jerky does not"))
            }
            (_, None) => {
                divergences.push(format!("jerky has an importer `{at}` and pnpm does not"))
            }
            (Some(mine), Some(theirs)) => {
                let names: BTreeSet<&String> = mine.keys().chain(theirs.keys()).collect();
                for name in names {
                    match (mine.get(name), theirs.get(name)) {
                        (Some(mine), Some(theirs)) if mine != theirs => divergences.push(format!(
                            "importer `{at}` links {name} to {mine}, pnpm to {theirs}"
                        )),
                        (Some(_), None) => divergences
                            .push(format!("importer `{at}` links a `{name}` pnpm does not")),
                        (None, Some(_)) => divergences
                            .push(format!("importer `{at}` links no `{name}`, pnpm does")),
                        _ => {}
                    }
                }
            }
        }
    }

    divergences
}

#[test]
fn the_oracle_loads_and_every_tree_in_it_has_a_recorded_answer() {
    // A silently empty fixture is this pattern's failure mode: every parity
    // assertion below would pass over nothing at all.
    let oracle = oracle();

    assert!(
        oracle.oracle.starts_with("pnpm "),
        "the recording does not say which pnpm took it: {:?}",
        oracle.oracle
    );
    assert!(!oracle.trees.is_empty(), "the oracle records no trees");

    // pnpm auto-installs a missing peer by default, and jerky never fabricates
    // an edge (spec §2). A recording taken with that setting on would answer a
    // different question, and every "unsatisfied" tree would quietly become a
    // satisfied one.
    assert!(
        oracle
            .settings
            .iter()
            .any(|s| s == "auto-install-peers=false"),
        "the recording was not taken with auto-install-peers off: {:?}",
        oracle.settings
    );

    for tree in &oracle.trees {
        assert!(
            !tree.packages.is_empty() && !tree.pnpm.nodes.is_empty(),
            "{}: recorded nothing",
            tree.name
        );
        assert!(
            tree.packages
                .iter()
                .any(|package| !package.peers.is_empty()),
            "{}: no package in it declares a peer, so it cannot be about peers",
            tree.name
        );
    }
}

#[test]
fn the_set_covers_every_phenomenon_and_each_tree_exhibits_the_one_it_claims() {
    // Two claims in one, and both are needed. That the four phenomena appear
    // is the coverage requirement; that each tree actually exhibits the label
    // it carries is what stops the first from being satisfied by a typo.
    let oracle = oracle();
    let mut covered = BTreeSet::new();

    for tree in &oracle.trees {
        assert!(
            exhibits(tree, tree.phenomenon),
            "{}: recorded graph does not exhibit {:?}",
            tree.name,
            tree.phenomenon
        );
        covered.insert(tree.phenomenon);
    }

    assert_eq!(
        covered,
        BTreeSet::from([
            Phenomenon::ImporterSatisfied,
            Phenomenon::AncestorSatisfied,
            Phenomenon::DuplicatedByPeers,
            Phenomenon::OptionalUnsatisfied,
        ]),
        "the set does not cover every phenomenon the oracle exists for"
    );
}

/// Does the recorded graph actually show this?
///
/// Read off pnpm's answer rather than off jerky's, so the coverage claim holds
/// whether or not jerky agrees — a phenomenon jerky gets wrong is exactly the
/// one the set must not lose.
fn exhibits(tree: &Tree, phenomenon: Phenomenon) -> bool {
    let published = |node: &Node| {
        tree.packages
            .iter()
            .find(|p| p.name == node.name && p.version == node.version)
            .expect("every node names a recorded package")
    };

    // Who answered a peer, matched against the edge's actual target rather
    // than against the name alone. "Some importer declares react" and "this
    // node's react came from an importer" are different claims, and only the
    // second is the phenomenon; the first would be satisfied by any tree that
    // happened to mention the name anywhere.
    let importer_answered = |name: &str, target: usize| {
        tree.pnpm
            .importers
            .values()
            .any(|links| links.get(name) == Some(&target))
    };
    let package_answered = |name: &str, target: usize| {
        tree.pnpm
            .nodes
            .iter()
            .any(|node| node.dependencies.get(name) == Some(&target))
    };

    // And the other half: that nothing of the *other* kind could have answered
    // it, which is what makes the two exclusive rather than merely different.
    let declared_by_importer = |name: &str| {
        tree.importers
            .values()
            .any(|section| section.contains_key(name))
    };
    let declared_by_package = |name: &str| {
        tree.packages
            .iter()
            .any(|package| package.dependencies.contains_key(name))
    };

    match phenomenon {
        Phenomenon::ImporterSatisfied => tree.pnpm.nodes.iter().any(|node| {
            node.peers.iter().any(|(peer, target)| {
                importer_answered(peer, *target) && !declared_by_package(peer)
            })
        }),
        Phenomenon::AncestorSatisfied => tree.pnpm.nodes.iter().any(|node| {
            node.peers.iter().any(|(peer, target)| {
                package_answered(peer, *target) && !declared_by_importer(peer)
            })
        }),
        Phenomenon::DuplicatedByPeers => tree.pnpm.nodes.iter().any(|node| {
            tree.pnpm.nodes.iter().any(|other| {
                other.name == node.name
                    && other.version == node.version
                    && other.peers != node.peers
            })
        }),
        Phenomenon::OptionalUnsatisfied => tree.pnpm.nodes.iter().any(|node| {
            published(node)
                .peers
                .iter()
                .any(|(peer, declared)| declared.optional && !node.peers.contains_key(peer))
        }),
    }
}

#[test]
fn jerky_resolves_every_recorded_tree_the_way_pnpm_did() {
    let oracle = oracle();
    let mut failures = Vec::new();

    for tree in &oracle.trees {
        for divergence in divergences(tree) {
            failures.push(format!("{}: {divergence}", tree.name));
        }
    }

    assert!(
        failures.is_empty(),
        "{} divergences from {}:\n{}",
        failures.len(),
        oracle.oracle,
        failures.join("\n")
    );
}

#[test]
fn a_deliberately_wrong_expectation_is_caught() {
    // Without this the parity test could be passing vacuously — comparing an
    // empty set with an empty set, or rendering both sides through a form too
    // coarse to tell a wrong answer from a right one. Each perturbation below
    // is a different way for the comparison to be blind, so each is made
    // separately rather than all at once.
    let oracle = oracle();

    let repointed = {
        // The duplicated package given the *other* importer's react. Both
        // copies then resolve identically, so pnpm's side loses a node — the
        // failure a comparison that only counted nodes per name would miss.
        let mut tree = tree_named(&oracle, "peer-duplication-across-importers");
        let react = tree.pnpm.importers["packages/react18"]["react"];
        let node = tree.pnpm.importers["packages/react17"]["use-sync-external-store"];
        tree.pnpm.nodes[node]
            .peers
            .insert("react".to_string(), react);
        tree
    };
    assert!(
        !divergences(&repointed).is_empty(),
        "a peer pointed at the wrong provider went unnoticed"
    );

    let dropped = {
        // A satisfied peer recorded as unsatisfied. The node still exists and
        // still has the right name and version; only what it resolved against
        // has gone.
        let mut tree = tree_named(&oracle, "peer-from-importer");
        for node in &mut tree.pnpm.nodes {
            node.peers.clear();
        }
        tree
    };
    assert!(
        !divergences(&dropped).is_empty(),
        "a peer edge deleted from the expectation went unnoticed"
    );

    let relinked = {
        // The importer pointed at a node that is in the graph but is not the
        // one it resolved. Nothing about the node set changes, so only the
        // importer half of the comparison can catch it.
        let mut tree = tree_named(&oracle, "peer-duplication-across-importers");
        let other = tree.pnpm.importers["packages/react18"]["use-sync-external-store"];
        tree.pnpm
            .importers
            .get_mut("packages/react17")
            .unwrap()
            .insert("use-sync-external-store".to_string(), other);
        tree
    };
    assert!(
        !divergences(&relinked).is_empty(),
        "an importer linked to the wrong copy went unnoticed"
    );

    let unlinked = {
        // A link jerky makes that the expectation does not. The other three
        // perturbations all take something away from jerky's side or move it;
        // this is the only one where jerky has *more*, and a comparison that
        // walked pnpm's importers and looked each up in jerky's would find
        // nothing wrong with it.
        let mut tree = tree_named(&oracle, "peer-duplication-across-importers");
        tree.pnpm
            .importers
            .get_mut("packages/react17")
            .unwrap()
            .remove("react");
        tree
    };
    assert!(
        !divergences(&unlinked).is_empty(),
        "an importer link jerky made and pnpm did not went unnoticed"
    );
}

fn tree_named(oracle: &Oracle, name: &str) -> Tree {
    oracle
        .trees
        .iter()
        .find(|tree| tree.name == name)
        .unwrap_or_else(|| panic!("the oracle has no tree `{name}`"))
        .clone()
}
