//! The resolver's tree-shape tests.
//!
//! Each one is a claim about the design rather than coverage for its own sake:
//! that a diamond dedupes, that a conflict deliberately does not, that a cycle
//! terminates, and that the walk is iterative.

use std::collections::BTreeMap;
use std::path::Path;

use jerky::registry::MAX_CONCURRENT_FETCHES;
use jerky::registry::RegistryError;
use jerky::resolver::{Declared, ImporterPath, Kind, Resolution, ResolveError, resolve};
use jerky::testing::FixtureRegistry;

/// A workspace of one, keyed `.` — the degenerate case of the general input,
/// which is why these tests read unchanged now that the resolver takes many.
fn roots(list: &[(&str, &str)]) -> BTreeMap<ImporterPath, BTreeMap<String, Declared>> {
    BTreeMap::from([(ImporterPath::root(), section(list, Kind::Prod))])
}

/// One manifest section as the resolver takes it.
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

#[test]
fn resolves_a_diamond_to_one_shared_node() {
    // a -> b, a -> c, b -> d, c -> d.  d must appear exactly once.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("d", "^1.0.0")]),
        ("c", "1.0.0", &[("d", "^1.0.0")]),
        ("d", "1.0.0", &[]),
    ]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

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

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

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

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

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

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

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

    let graph = resolve(&registry, &roots(&[("p0", "^1.0.0")]), &no_members()).unwrap();

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

    resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

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

    resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

    assert_eq!(registry.packument_calls_for("d"), 1);
}

#[test]
fn an_unsatisfiable_range_names_the_available_versions() {
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);

    let err = resolve(&registry, &roots(&[("a", "^9.0.0")]), &no_members()).unwrap_err();

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
        resolve(&registry, &roots(&[("nope", "^1.0.0")]), &no_members()),
        Err(ResolveError::Registry(_))
    ));
}

#[test]
fn dist_tags_resolve_through_the_packument() {
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[]), ("a", "2.0.0", &[])]);

    let graph = resolve(&registry, &roots(&[("a", "latest")]), &no_members()).unwrap();

    assert_eq!(graph.packages.len(), 1);
    assert_eq!(graph.packages.keys().next().unwrap().version, "2.0.0");
}

#[test]
fn the_root_importer_records_what_was_asked_and_what_was_chosen() {
    // The specifier is what makes staleness detectable; the resolution is what
    // saves linking from re-deriving which version a range picked.
    use jerky::resolver::{ImporterPath, Resolution};

    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

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

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

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

    let first = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
    let second = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

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

    fn packument_conditional(
        &self,
        name: &str,
        _etag: Option<&str>,
    ) -> Result<jerky::registry::Fetched, jerky::registry::RegistryError> {
        use jerky::registry::{Dist, Fetched, Packument, VersionMetadata};

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

        Ok(Fetched::Body {
            packument: Box::new(Packument {
                name: name.to_string(),
                versions,
                dist_tags: self
                    .tags
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }),
            etag: None,
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

    let err = resolve(&registry, &roots(&[("a", "latest")]), &no_members()).unwrap_err();

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

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

    let chosen = &graph.packages.keys().next().unwrap().version;
    assert_eq!(chosen, "1.5.0", "the tag overrode the range");
}

#[test]
fn a_spec_that_is_neither_a_range_nor_a_tag_is_reported() {
    let registry = HandBuilt {
        versions: vec!["1.0.0"],
        tags: vec![("latest", "1.0.0"), ("next", "1.0.0")],
    };

    let err = resolve(&registry, &roots(&[("a", "nonsense-spec")]), &no_members()).unwrap_err();

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

fn importers(
    list: &[(&str, &[(&str, &str)])],
) -> BTreeMap<ImporterPath, BTreeMap<String, Declared>> {
    list.iter()
        .map(|(importer, deps)| {
            (
                ImporterPath::new(*importer).unwrap(),
                section(deps, Kind::Prod),
            )
        })
        .collect()
}

#[test]
fn each_importer_records_the_version_it_asked_for() {
    // Two importers wanting incompatible majors. The resolver deliberately
    // does not force agreement, so both land and each importer points at its
    // own — the cross-tree conflict case, now across projects.
    let registry =
        FixtureRegistry::new().with_tree(&[("lodash", "4.17.21", &[]), ("lodash", "3.10.1", &[])]);

    let graph = resolve(
        &registry,
        &importers(&[
            ("packages/ui", &[("lodash", "^4.0.0")]),
            ("apps/web", &[("lodash", "^3.0.0")]),
        ]),
        &no_members(),
    )
    .unwrap();

    assert_eq!(graph.importers.len(), 2, "an importer went missing");
    for (importer, expected) in [("packages/ui", "4.17.21"), ("apps/web", "3.10.1")] {
        let key = ImporterPath::new(importer).unwrap();
        let dep = &graph.importers[&key].dependencies["lodash"];
        match &dep.resolution {
            Resolution::Registry(id) => assert_eq!(id.version, expected, "{importer}"),
            other => panic!("{importer} resolved to {other:?}, not the registry"),
        }
    }
    assert_eq!(graph.packages.len(), 2, "both versions must coexist");
}

#[test]
fn one_walk_covers_importers_that_agree() {
    // Two importers on the same range share the work: the packument is
    // fetched once, not once per importer.
    let registry = FixtureRegistry::new().with_tree(&[("lodash", "4.17.21", &[])]);

    let graph = resolve(
        &registry,
        &importers(&[
            ("packages/ui", &[("lodash", "^4.0.0")]),
            ("apps/web", &[("lodash", "^4.0.0")]),
        ]),
        &no_members(),
    )
    .unwrap();

    assert_eq!(graph.packages.len(), 1, "the shared version was duplicated");
    assert_eq!(
        registry.packument_calls_for("lodash"),
        1,
        "each importer fetched the packument separately"
    );
}

/// A workspace whose members declare nothing local, which is every test that
/// predates the `workspace:` protocol.
fn no_members() -> BTreeMap<String, ImporterPath> {
    BTreeMap::new()
}

fn members(list: &[(&str, &str)]) -> BTreeMap<String, ImporterPath> {
    list.iter()
        .map(|(name, importer)| (name.to_string(), ImporterPath::new(*importer).unwrap()))
        .collect()
}

#[test]
fn a_workspace_specifier_never_reaches_the_registry() {
    // The member is in the repo, so there is nothing to fetch and nothing to
    // verify. An empty registry proves the short-circuit rather than merely
    // suggesting it.
    let registry = FixtureRegistry::new();

    let graph = resolve(
        &registry,
        &importers(&[("apps/web", &[("ui", "workspace:*")])]),
        &members(&[("ui", "packages/ui")]),
    )
    .unwrap();

    let dep = &graph.importers[&ImporterPath::new("apps/web").unwrap()].dependencies["ui"];
    match &dep.resolution {
        // Relative to the importer that declared it: apps/web climbs two to
        // the workspace root, then descends.
        Resolution::Local(path) => assert_eq!(path, Path::new("../../packages/ui")),
        other => panic!("resolved to {other:?}, not a local member"),
    }
    assert_eq!(
        dep.specifier, "workspace:*",
        "the specifier is recorded as written"
    );
    assert_eq!(graph.packages.len(), 0);
    assert_eq!(registry.packument_calls(), 0, "the registry was consulted");
}

#[test]
fn a_workspace_specifier_from_the_root_importer_descends_only() {
    let registry = FixtureRegistry::new();

    let graph = resolve(
        &registry,
        &importers(&[(".", &[("ui", "workspace:^1.0.0")])]),
        &members(&[("ui", "packages/ui")]),
    )
    .unwrap();

    let dep = &graph.importers[&ImporterPath::root()].dependencies["ui"];
    match &dep.resolution {
        Resolution::Local(path) => assert_eq!(path, Path::new("packages/ui")),
        other => panic!("resolved to {other:?}, not a local member"),
    }
}

#[test]
fn a_workspace_specifier_naming_no_member_is_an_error() {
    // `workspace:*` for a package that is not in the repo is a typo, not a
    // fallback to the registry.
    let registry = FixtureRegistry::new().with_tree(&[("ui", "1.0.0", &[])]);

    let err = resolve(
        &registry,
        &importers(&[("apps/web", &[("ui", "workspace:*")])]),
        &members(&[("other", "packages/other")]),
    )
    .unwrap_err();

    match err {
        ResolveError::NoSuchMember { name, members, .. } => {
            assert_eq!(name, "ui");
            assert!(
                members.contains(&"other".to_string()),
                "the error names the members"
            );
        }
        other => panic!("expected NoSuchMember, got {other:?}"),
    }
    assert_eq!(
        registry.packument_calls(),
        0,
        "a missing member fell back to the registry instead of failing"
    );
}

/// A registry serving packuments straight from raw JSON.
///
/// The only way to express a package that *declares* devDependencies:
/// `VersionMetadata` has no field for them, so `FixtureRegistry` cannot build
/// one, and a test that cannot express the input cannot test the defence. The
/// JSON below is the shape npm actually serves — a live `express` version
/// really does carry a `devDependencies` object.
struct RawRegistry {
    packuments: BTreeMap<String, String>,
}

impl jerky::registry::RegistryClient for RawRegistry {
    fn version_metadata(
        &self,
        name: &str,
        _version: &str,
    ) -> Result<jerky::registry::VersionMetadata, jerky::registry::RegistryError> {
        Err(jerky::registry::RegistryError::PackageNotFound(
            name.to_string(),
        ))
    }

    fn packument_conditional(
        &self,
        name: &str,
        _etag: Option<&str>,
    ) -> Result<jerky::registry::Fetched, jerky::registry::RegistryError> {
        let raw = self
            .packuments
            .get(name)
            .ok_or_else(|| jerky::registry::RegistryError::PackageNotFound(name.to_string()))?;
        Ok(jerky::registry::Fetched::Body {
            packument: Box::new(
                serde_json::from_str(raw).expect("the fixture JSON is well formed"),
            ),
            etag: None,
        })
    }

    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>, jerky::registry::RegistryError> {
        panic!("resolution fetched {url}; it does no I/O beyond metadata");
    }
}

/// One packument holding one version, with whatever sections the caller names.
fn raw_packument(name: &str, sections: &str) -> String {
    format!(
        r#"{{"name":"{name}","dist-tags":{{"latest":"1.0.0"}},
            "versions":{{"1.0.0":{{"name":"{name}","version":"1.0.0",
                "dist":{{"tarball":"https://r.test/{name}.tgz",
                         "integrity":"{}"}}{sections}}}}}}}"#,
        jerky::testing::ABC_SHA512_SSRI
    )
}

#[test]
fn a_registry_packages_dev_dependencies_are_still_not_followed() {
    // The correctness requirement, and the one widening the resolver's input
    // could plausibly have broken: an importer's devDependencies are followed
    // now, and a package's must never be — following them pulls in most of the
    // registry. `a` is declared as a devDependency here precisely so the two
    // rules meet in one graph.
    //
    // Enforced by `VersionMetadata` having no field for them rather than by
    // the resolver remembering, which is why `test-only-dep` is a package the
    // registry could serve: this fails because the graph holds it, not because
    // resolution errored looking for something absent.
    let registry = RawRegistry {
        packuments: BTreeMap::from([
            (
                "a".to_string(),
                raw_packument(
                    "a",
                    r#","dependencies":{"runtime-dep":"^1.0.0"},
                       "devDependencies":{"test-only-dep":"^1.0.0"}"#,
                ),
            ),
            ("runtime-dep".to_string(), raw_packument("runtime-dep", "")),
            (
                "test-only-dep".to_string(),
                raw_packument("test-only-dep", ""),
            ),
        ]),
    };

    let graph = resolve(
        &registry,
        &BTreeMap::from([(
            ImporterPath::new("packages/ui").unwrap(),
            section(&[("a", "^1.0.0")], Kind::Dev),
        )]),
        &no_members(),
    )
    .unwrap();

    let names: Vec<&str> = graph.packages.keys().map(|id| id.name.as_str()).collect();
    assert_eq!(
        names,
        ["a", "runtime-dep"],
        "a dependency's devDependencies were followed"
    );

    // And the importer's own devDependency did resolve, so the assertion above
    // is not passing because nothing happened at all.
    let dependency = &graph.importers[&ImporterPath::new("packages/ui").unwrap()].dependencies["a"];
    assert_eq!(dependency.kind, Kind::Dev);
}

/// `count` leaves with no dependencies of their own, plus the declaration that
/// asks for all of them.
///
/// They are declared by the *importer* rather than by a package, so the whole
/// fan-out is the walk's first level. A root package above them would put a
/// level of exactly one ahead of it, and a rendezvous cannot be met by one
/// caller.
fn fan_out(count: usize) -> (FixtureRegistry, Vec<(String, String)>) {
    let mut registry = FixtureRegistry::new();
    let mut declared = Vec::new();
    for i in 0..count {
        let name = format!("leaf{i:02}");
        registry = registry.with_tree(&[(name.as_str(), "1.0.0", &[])]);
        declared.push((name, "^1.0.0".to_string()));
    }
    (registry, declared)
}

/// The borrowed view of `fan_out`'s declarations that `roots` takes.
fn as_roots(declared: &[(String, String)]) -> BTreeMap<ImporterPath, BTreeMap<String, Declared>> {
    let list: Vec<(&str, &str)> = declared
        .iter()
        .map(|(name, range)| (name.as_str(), range.as_str()))
        .collect();
    roots(&list)
}

#[test]
fn packuments_on_one_level_are_fetched_concurrently() {
    // Four siblings, and a rendezvous four wide. A resolver that asks the
    // registry for one packument at a time can never have four in flight, so
    // it blocks until the rendezvous gives up and reports itself broken.
    let (registry, declared) = fan_out(4);
    let registry = registry.with_packument_rendezvous(4);

    resolve(&registry, &as_roots(&declared), &no_members()).unwrap();

    assert!(
        registry.packuments_met_rendezvous(),
        "four sibling packuments never overlapped, so resolution is still serial"
    );
}

#[test]
fn a_level_wider_than_the_cap_still_reaches_it() {
    // Twice the cap of work in one level. The rendezvous proves the pool
    // reaches its own width, which is the half of "bounded at sixteen" that
    // can be proven without waiting for a timeout.
    let (registry, declared) = fan_out(MAX_CONCURRENT_FETCHES * 2);
    let registry = registry.with_packument_rendezvous(MAX_CONCURRENT_FETCHES);

    resolve(&registry, &as_roots(&declared), &no_members()).unwrap();

    assert!(
        registry.packuments_met_rendezvous(),
        "the pool never reached its own cap, so the cap is not the limit in force"
    );

    // Necessary but not sufficient, and deliberately so. `peak_concurrent_
    // packuments` is exact, so a peak above the cap is a real failure — but an
    // unbounded implementation is not *guaranteed* to be caught here, since it
    // would have to be observed above the cap rather than merely be capable of
    // it. Proving the upper bound outright means a rendezvous one wider than
    // the cap, asserted to time out, and that costs `RENDEZVOUS_TIMEOUT` on
    // every run.
    assert!(
        registry.peak_concurrent_packuments() <= MAX_CONCURRENT_FETCHES,
        "fetched {} packuments at once, above the cap of {}",
        registry.peak_concurrent_packuments(),
        MAX_CONCURRENT_FETCHES
    );
}

#[test]
fn a_package_two_dependents_share_is_still_fetched_once() {
    // The memo that stops a diamond being fetched twice is the thing
    // concurrency most easily breaks: two workers both miss, both fetch, both
    // insert. `d` is named by `b` and `c`, which sit on the same level.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("d", "^1.0.0")]),
        ("c", "1.0.0", &[("d", "^1.0.0")]),
        ("d", "1.0.0", &[]),
    ]);

    resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

    assert_eq!(
        registry.packument_calls_for("d"),
        1,
        "the shared dependency was fetched once per dependent"
    );
}

/// A tree with depth, diamonds, two versions of one name and two importers —
/// so every level is discovered from the one above rather than handed over
/// whole, which is the only situation in which completion order could leak
/// anywhere.
fn tangled() -> FixtureRegistry {
    FixtureRegistry::new().with_tree(&[
        ("app", "1.0.0", &[("left", "^1.0.0"), ("right", "^1.0.0")]),
        (
            "left",
            "1.0.0",
            &[("shared", "^1.0.0"), ("l1", "^1.0.0"), ("l2", "^1.0.0")],
        ),
        (
            "right",
            "1.0.0",
            &[("shared", "^1.0.0"), ("r1", "^1.0.0"), ("r2", "^2.0.0")],
        ),
        ("shared", "1.0.0", &[("deep", "^1.0.0")]),
        ("l1", "1.0.0", &[("deep", "^1.0.0")]),
        ("l2", "1.0.0", &[]),
        ("r1", "1.0.0", &[("deep", "^1.0.0")]),
        ("r2", "1.0.0", &[]),
        ("r2", "2.0.0", &[]),
        ("deep", "1.0.0", &[("leaf", "^1.0.0")]),
        ("leaf", "1.0.0", &[]),
    ])
}

/// Everything the lockfile would be written from, as one comparable string.
fn shape(graph: &jerky::resolver::ResolvedGraph) -> String {
    let mut out = String::new();
    for (path, importer) in &graph.importers {
        for (name, dependency) in &importer.dependencies {
            let resolution = match &dependency.resolution {
                Resolution::Registry(id) => id.to_string(),
                Resolution::Local(target) => format!("link:{}", target.display()),
            };
            out.push_str(&format!(
                "{}|{name}|{}|{resolution}\n",
                path.as_str(),
                dependency.specifier
            ));
        }
    }
    for (id, package) in &graph.packages {
        out.push_str(&format!("{id}|{}|", package.resolved));
        for (name, to) in &package.dependencies {
            out.push_str(&format!("{name}->{to},"));
        }
        out.push('\n');
    }
    out
}

#[test]
fn the_same_tree_resolves_identically_every_time() {
    // Determinism is what `BTreeMap` everywhere that reaches disk exists to
    // guarantee, and the property concurrency threatens by letting the order
    // fetches happen to complete in decide anything. The comparison is over
    // everything the lockfile is written from — importer edges, the specifier
    // recorded beside each, every package, its tarball URL and its own edges —
    // rather than over the package set, which a `BTreeMap` would hold steady
    // on its own and which would make this pass against anything.
    let roots = BTreeMap::from([
        (
            ImporterPath::root(),
            section(&[("app", "^1.0.0")], Kind::Prod),
        ),
        (
            ImporterPath::new("packages/ui").unwrap(),
            section(&[("left", "^1.0.0")], Kind::Dev),
        ),
    ]);

    let first = shape(&resolve(&tangled(), &roots, &no_members()).unwrap());
    for _ in 0..20 {
        let again = shape(&resolve(&tangled(), &roots, &no_members()).unwrap());
        assert_eq!(again, first, "the resolved graph varied between runs");
    }

    // And the walk really did descend, so the assertion above is not holding
    // because resolution quietly stopped early. `leaf` sits four levels below
    // the importers and is reachable only through them.
    assert!(
        first.contains("leaf@1.0.0"),
        "the walk never reached the deepest package: {first}"
    );
}

#[test]
fn a_missing_package_is_named_and_is_the_same_one_every_time() {
    // "One of four lookups failed" is not a diagnosable error, and a run that
    // blamed a different package each time is worse than one that blames the
    // wrong one consistently.
    //
    // The two missing names are ordered so that the walk's answer and the
    // alphabetical answer differ: `alpha` is visited first and asks for
    // `zzz-gone`, `beta` second and asks for `aaa-gone`, so the level reads
    // [zzz-gone, aaa-gone] while sorted order reads the reverse. A pass that
    // reported whichever failure its own ordering reached first would report
    // `aaa-gone` and be caught here.
    let mut reported = std::collections::BTreeSet::new();
    for _ in 0..20 {
        let registry = FixtureRegistry::new().with_tree(&[
            ("alpha", "1.0.0", &[("zzz-gone", "^1.0.0")]),
            ("beta", "1.0.0", &[("aaa-gone", "^1.0.0")]),
        ]);

        let err = resolve(
            &registry,
            &roots(&[("alpha", "^1.0.0"), ("beta", "^1.0.0")]),
            &no_members(),
        )
        .unwrap_err();

        match err {
            ResolveError::Registry(source) => {
                reported.insert(source.to_string());
            }
            other => panic!("expected a registry failure naming the package, got {other:?}"),
        }
    }

    assert_eq!(
        reported.len(),
        1,
        "the reported failure varied between runs: {reported:?}"
    );
    let only = reported.iter().next().unwrap();
    assert!(
        only.contains("zzz-gone"),
        "the error did not name the package the walk reaches first: {only}"
    );
}

#[test]
fn an_alias_resolves_the_package_it_names_under_the_key_it_was_given() {
    // `@isaacs/cliui` declares `string-width-cjs: npm:string-width@^4.2.0`, so
    // a name jerky must ask the registry about is not the name the edge is
    // recorded under. Nothing in the registry answers to `width-cjs`.
    let registry = FixtureRegistry::new().with_tree(&[
        (
            "cliui",
            "1.0.0",
            &[("width-cjs", "npm:string-width@^4.0.0")],
        ),
        ("string-width", "4.2.3", &[]),
    ]);

    let graph = resolve(&registry, &roots(&[("cliui", "^1.0.0")]), &no_members()).unwrap();

    let cliui = graph
        .packages
        .values()
        .find(|package| package.id.name == "cliui")
        .expect("cliui resolved");
    let aliased = cliui
        .dependencies
        .get("width-cjs")
        .expect("the edge is recorded under the name the manifest used");
    assert_eq!(aliased.name, "string-width");
    assert_eq!(aliased.version, "4.2.3");

    // And the node it points at is the real package, keyed by its own name —
    // not a `width-cjs@4.2.3` that no registry could serve.
    assert!(
        graph.packages.contains_key(aliased),
        "the alias target is not a node in the graph: {:?}",
        graph.packages.keys().collect::<Vec<_>>()
    );
    assert!(
        !graph.packages.keys().any(|id| id.name == "width-cjs"),
        "the local name became a package of its own"
    );
}

#[test]
fn two_names_for_one_package_resolve_to_one_node() {
    // The point of keying selection on what was *asked for* rather than on the
    // name it was asked under: one entry in the store, one download, however
    // many local names reach it.
    let registry = FixtureRegistry::new().with_tree(&[
        (
            "app",
            "1.0.0",
            &[
                ("width-cjs", "npm:string-width@^4.0.0"),
                ("width-legacy", "npm:string-width@^4.0.0"),
                ("string-width", "^4.0.0"),
            ],
        ),
        ("string-width", "4.2.3", &[]),
    ]);

    let graph = resolve(&registry, &roots(&[("app", "^1.0.0")]), &no_members()).unwrap();

    let widths: Vec<_> = graph
        .packages
        .keys()
        .filter(|id| id.name == "string-width")
        .collect();
    assert_eq!(widths.len(), 1, "one package became {}", widths.len());
    assert_eq!(
        registry.packument_calls_for("string-width"),
        1,
        "the same package was fetched once per name it was asked under"
    );
    assert_eq!(registry.packument_calls_for("width-cjs"), 0);
}

#[test]
fn an_importers_own_alias_records_the_specifier_verbatim() {
    // jerky pins exact, so the *resolution* is the concrete version — but the
    // specifier recorded beside it is the whole `npm:` string, because that is
    // what the manifest says and what staleness is measured against. A
    // normalised `^4.0.0` would read as unchanged after an edit that changed
    // which package is being aliased.
    let registry = FixtureRegistry::new().with_tree(&[("string-width", "4.2.3", &[])]);

    let graph = resolve(
        &registry,
        &roots(&[("width-cjs", "npm:string-width@^4.0.0")]),
        &no_members(),
    )
    .unwrap();

    let dependency = &graph.importers[&ImporterPath::root()].dependencies["width-cjs"];
    assert_eq!(dependency.specifier, "npm:string-width@^4.0.0");
    match &dependency.resolution {
        Resolution::Registry(id) => {
            assert_eq!(id.name, "string-width");
            assert_eq!(id.version, "4.2.3");
        }
        other => panic!("expected a registry resolution, got {other:?}"),
    }
}

#[test]
fn an_alias_without_a_version_takes_the_latest_tag() {
    let registry = FixtureRegistry::new().with_tree(&[("string-width", "4.2.3", &[])]);

    let graph = resolve(
        &registry,
        &roots(&[("width-cjs", "npm:string-width")]),
        &no_members(),
    )
    .unwrap();

    match &graph.importers[&ImporterPath::root()].dependencies["width-cjs"].resolution {
        Resolution::Registry(id) => assert_eq!(id.to_string(), "string-width@4.2.3"),
        other => panic!("expected a registry resolution, got {other:?}"),
    }
}

#[test]
fn a_scoped_package_can_be_aliased() {
    // The alias target splits on its *last* `@` for the same reason a lockfile
    // key does: the first one is the scope.
    let registry = FixtureRegistry::new()
        .with_tree(&[("@scope/real", "2.0.0", &[]), ("caller", "1.0.0", &[])])
        .with_tree(&[("caller", "1.0.0", &[("local", "npm:@scope/real@^2.0.0")])]);

    let graph = resolve(&registry, &roots(&[("caller", "^1.0.0")]), &no_members()).unwrap();

    let caller = graph
        .packages
        .values()
        .find(|package| package.id.name == "caller")
        .expect("caller resolved");
    assert_eq!(caller.dependencies["local"].name, "@scope/real");
    assert_eq!(caller.dependencies["local"].version, "2.0.0");
}

#[test]
fn an_unsupported_scheme_says_so_rather_than_blaming_the_range() {
    // `file:`, `git:` and `github:` are all real and none are supported. The
    // failure a user sees should name the reason, not report a perfectly
    // well-formed specifier as a bad version range — which is what jerky did
    // for `npm:` before it understood one.
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);

    let err = resolve(
        &registry,
        &roots(&[("a", "github:expressjs/express")]),
        &no_members(),
    )
    .unwrap_err();

    match err {
        ResolveError::UnsupportedScheme {
            name,
            specifier,
            scheme,
        } => {
            assert_eq!(name, "a");
            assert_eq!(specifier, "github:expressjs/express");
            assert_eq!(scheme, "github");
        }
        other => panic!("expected an unsupported-scheme error, got {other:?}"),
    }
}

#[test]
fn an_alias_naming_nothing_is_still_a_missing_package() {
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);

    let err = resolve(
        &registry,
        &roots(&[("local", "npm:not-published@^1.0.0")]),
        &no_members(),
    )
    .unwrap_err();

    // Matched on the variant rather than the message: the specifier itself
    // contains "not-published", so an error that merely echoed what was
    // declared would satisfy a substring check while proving nothing.
    match err {
        ResolveError::Registry(RegistryError::PackageNotFound(missing)) => {
            assert_eq!(missing, "not-published")
        }
        other => panic!("expected the aliased package to be reported missing, got {other:?}"),
    }
}

#[test]
fn an_alias_naming_no_package_is_malformed_rather_than_unsupported() {
    // `npm:` is the one scheme jerky understands, so "jerky does not
    // understand `npm:` specifiers" is the wrong complaint about every one of
    // these. `npm:@1.0.0` is the subtle one: the leading `@` reads as a scope,
    // so the target parses as a package named `@1.0.0` and would otherwise be
    // sent to the registry as a name.
    for specifier in ["npm:", "npm:@1.0.0", "npm:@scope", "npm:@/pkg"] {
        let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);
        let err = resolve(&registry, &roots(&[("local", specifier)]), &no_members()).unwrap_err();

        match err {
            ResolveError::MalformedAlias {
                name,
                specifier: got,
            } => {
                assert_eq!(name, "local");
                assert_eq!(got, specifier);
            }
            other => panic!("expected `{specifier}` to be malformed, got {other:?}"),
        }
    }
}

#[test]
fn two_importers_aliasing_one_package_share_its_node() {
    // The workspace form of dedup, and the one #73 asked for: two members,
    // two different local names, one package. A resolver keyed on the name a
    // dependency was declared under would give each its own node, its own
    // store entry and its own download.
    let registry = FixtureRegistry::new().with_tree(&[("string-width", "4.2.3", &[])]);

    let roots = BTreeMap::from([
        (
            ImporterPath::new("apps/web").unwrap(),
            section(&[("width-cjs", "npm:string-width@^4.0.0")], Kind::Prod),
        ),
        (
            ImporterPath::new("packages/ui").unwrap(),
            section(&[("width-legacy", "npm:string-width@^4.0.0")], Kind::Prod),
        ),
    ]);

    let graph = resolve(&registry, &roots, &no_members()).unwrap();

    assert_eq!(
        graph.packages.len(),
        1,
        "one package resolved to {} nodes",
        graph.packages.len()
    );
    assert_eq!(registry.packument_calls_for("string-width"), 1);

    // Both importers point at the same node under their own chosen names.
    for (path, local) in [("apps/web", "width-cjs"), ("packages/ui", "width-legacy")] {
        let importer = &graph.importers[&ImporterPath::new(path).unwrap()];
        match &importer.dependencies[local].resolution {
            Resolution::Registry(id) => assert_eq!(id.to_string(), "string-width@4.2.3"),
            other => panic!("expected a registry resolution, got {other:?}"),
        }
    }
}

/// The freshness rule, end to end through a real resolve.
///
/// Unit tests cover the cache's policy in isolation; these prove the resolver
/// actually asks for what it should. The distinction matters because the
/// requirement is computed from the *specifier*, several layers above the
/// thing that honours it.
mod freshness {
    use super::*;

    use std::time::Duration;

    use jerky::metadata_cache::{CachedRegistry, DEFAULT_WINDOW, MetadataCache};
    use jerky::testing::FixtureRegistry;

    fn fixture() -> FixtureRegistry {
        FixtureRegistry::new().with_packument("a", &[("1.0.0", &[]), ("2.0.0", &[])])
    }

    fn cached(dir: &tempfile::TempDir, window: Duration) -> CachedRegistry<FixtureRegistry> {
        CachedRegistry::new(fixture(), MetadataCache::new(dir.path(), window))
    }

    #[test]
    fn a_range_is_answered_from_the_cache_on_the_second_resolve() {
        // The whole point: an install that re-resolves an unchanged manifest
        // inside the window reaches the registry not at all.
        let dir = tempfile::TempDir::new().unwrap();

        let first = cached(&dir, DEFAULT_WINDOW);
        resolve(&first, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
        assert_eq!(
            first.inner().packument_calls_for("a"),
            1,
            "a cold cache must fetch"
        );

        // A second resolver over the same cache directory, standing in for a
        // second `jerky install` in the same project.
        let second = cached(&dir, DEFAULT_WINDOW);
        resolve(&second, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
        assert_eq!(
            second.inner().packument_calls_for("a"),
            0,
            "a range inside the window must not reach the registry"
        );
    }

    #[test]
    fn a_dist_tag_reaches_the_registry_even_with_a_warm_cache() {
        // jerky promises that only the registry can say what `latest` means
        // today. The window must not quietly answer that question.
        let dir = tempfile::TempDir::new().unwrap();

        let first = cached(&dir, DEFAULT_WINDOW);
        resolve(&first, &roots(&[("a", "latest")]), &no_members()).unwrap();
        assert_eq!(first.inner().packument_calls_for("a"), 1);

        let second = cached(&dir, DEFAULT_WINDOW);
        resolve(&second, &roots(&[("a", "latest")]), &no_members()).unwrap();
        assert_eq!(
            second.inner().packument_calls_for("a"),
            1,
            "a dist-tag must ask every time, warm cache or not"
        );
    }

    #[test]
    fn a_cached_range_does_not_satisfy_a_later_dist_tag() {
        // The memo case. One packument serves every edge that names the
        // package, so an entry fetched for a range must not be handed to a tag
        // that demanded the registry — within one run or across two.
        let dir = tempfile::TempDir::new().unwrap();

        let ranged = cached(&dir, DEFAULT_WINDOW);
        resolve(&ranged, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
        assert_eq!(ranged.inner().packument_calls_for("a"), 1);

        let tagged = cached(&dir, DEFAULT_WINDOW);
        resolve(&tagged, &roots(&[("a", "latest")]), &no_members()).unwrap();
        assert_eq!(
            tagged.inner().packument_calls_for("a"),
            1,
            "a warm entry fetched for a range must not answer a dist-tag"
        );
    }
}
