#!/usr/bin/env node
// Regenerates `tests/fixtures/pnpm-peer-oracle.json`, which `tests/pnpm_peer_oracle.rs` reads.
//
//     node tests/fixtures/generate-pnpm-peer-oracle.mjs
//
// Peer resolution is the one part of jerky whose correct answer is defined by
// another implementation's behaviour rather than by a written specification, so
// the fixture tests in `tests/peers.rs` can only prove jerky matches the spec
// document's *reading* of the rule. This is what tests the reading: pnpm
// resolves a handful of real trees, and the committed fixture records what it
// decided. Generation reaches the network; the test that reads the fixture
// never does.
//
// A failure in the Rust test is never fixed by editing the fixture by hand.
// The fixture is a recording of pnpm's behaviour — re-record it by running
// this, and read the diff.
//
// pnpm arrives through corepack, at the version pinned below, so this needs no
// pnpm on PATH and two machines record the same answers. Bumping PNPM is a
// deliberate act: it can legitimately change the recording, and the diff is
// the point.

import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const PNPM = "pnpm@10.18.0";
const JS_YAML = "4.1.0";
const REGISTRY = "https://registry.npmjs.org";

const OUT = path.join(path.dirname(fileURLToPath(import.meta.url)), "pnpm-peer-oracle.json");

// The settings that make pnpm resolve peers the way jerky's spec says they
// resolve. `auto-install-peers` is the load-bearing one and defaults to *true*
// since pnpm 8: left alone, pnpm invents a dependency for every unsatisfied
// peer, which is the one thing §2 settled jerky will never do — every
// "unsatisfied" phenomenon below would silently become a satisfied one.
// `dedupe-peer-dependents` is pnpm's optimisation for collapsing copies across
// importers, which jerky has no equivalent of; off, so a difference in node
// count is jerky's to explain rather than a setting's.
const NPMRC = [
  "auto-install-peers=false",
  "dedupe-peer-dependents=false",
  "strict-peer-dependencies=false",
  "resolution-mode=highest",
  "",
].join("\n");

// Each tree is here for a phenomenon, not for size: four small trees
// exercising four code paths beat one large one exercising a single path.
// `phenomenon` is checked against the recorded graph by the Rust test rather
// than trusted, so a mislabelled tree fails rather than faking coverage.
const TREES = [
  {
    name: "peer-from-importer",
    phenomenon: "importer-satisfied",
    // `react-dom` peers on `react` and nothing in the tree depends on `react`,
    // so the importer's own dependency is the only thing that can answer.
    importers: { ".": { react: "18.3.1", "react-dom": "18.3.1" } },
  },
  {
    name: "peer-from-intermediate-ancestor",
    phenomenon: "ancestor-satisfied",
    // `update-browserslist-db` peers on `browserslist`; the importer declares
    // neither. `@babel/helper-compilation-targets` depends on `browserslist`,
    // so the answer has to come from an ancestor partway down. The peer also
    // closes a cycle — `browserslist` depends on `update-browserslist-db` —
    // which is the case a naive walk does not return from.
    importers: { ".": { "@babel/helper-compilation-targets": "7.29.7" } },
  },
  {
    name: "peer-duplication-across-importers",
    phenomenon: "duplicated-by-peers",
    // One published `use-sync-external-store@1.2.2`, two importers giving it
    // different reacts. Its peer range admits both, so nothing here is a
    // version conflict — the two copies exist only because their peers differ,
    // which is the whole reason a peer belongs in a package's identity.
    root: "@oracle/workspace",
    importers: {
      ".": {},
      "packages/react17": { react: "17.0.2", "use-sync-external-store": "1.2.2" },
      "packages/react18": { react: "18.3.1", "use-sync-external-store": "1.2.2" },
    },
    members: {
      "packages/react17": "@oracle/react17",
      "packages/react18": "@oracle/react18",
    },
  },
  {
    name: "optional-peer-unsatisfied",
    phenomenon: "optional-unsatisfied",
    // Both `@rollup/plugin-node-resolve` and its own `@rollup/pluginutils`
    // declare `rollup` an optional peer, and no rollup is installed. The whole
    // tree therefore carries no peer context at all, which is the shape that
    // catches a pass that keys a node on a peer it did not resolve.
    importers: { ".": { "@rollup/plugin-node-resolve": "15.3.0" } },
  },
];

function pnpm(args, cwd) {
  execFileSync("corepack", [PNPM, ...args], {
    cwd,
    stdio: ["ignore", "ignore", "inherit"],
    env: { ...process.env, COREPACK_ENABLE_DOWNLOAD_PROMPT: "0" },
  });
}

/// `@babel/core@7.29.7(x@1)` -> `@babel/core@7.29.7`.
function published(ref) {
  const suffix = ref.indexOf("(");
  return suffix === -1 ? ref : ref.slice(0, suffix);
}

/// `@babel/core@7.29.7` -> `["@babel/core", "7.29.7"]`. The leading `@` of a
/// scope is not a separator, which is why this searches from the right.
function split(key) {
  const at = key.lastIndexOf("@");
  if (at <= 0) throw new Error(`malformed package key \`${key}\``);
  return [key.slice(0, at), key.slice(at + 1)];
}

const metadataCache = new Map();

/// A version's manifest as the registry published it.
///
/// pnpm's lockfile records what each dependency *resolved to* but not the
/// range that asked for it, and the range is what the fixture registry has to
/// serve for jerky to reach the same answer by its own route.
async function manifest(key) {
  if (!metadataCache.has(key)) {
    const [name, version] = split(key);
    const response = await fetch(`${REGISTRY}/${name}/${version}`);
    if (!response.ok) throw new Error(`${name}@${version}: registry said ${response.status}`);
    metadataCache.set(key, await response.json());
  }
  return metadataCache.get(key);
}

function sorted(entries) {
  return Object.fromEntries([...entries].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
}

/// Resolve one tree with pnpm and read its lockfile.
function resolveWithPnpm(tree, work, yaml) {
  const dir = path.join(work, tree.name);
  mkdirSync(dir, { recursive: true });
  writeFileSync(path.join(dir, ".npmrc"), NPMRC);

  const members = tree.members ?? {};
  for (const [importer, dependencies] of Object.entries(tree.importers)) {
    const at = importer === "." ? dir : path.join(dir, importer);
    mkdirSync(at, { recursive: true });
    writeFileSync(
      path.join(at, "package.json"),
      JSON.stringify(
        {
          name: importer === "." ? (tree.root ?? `oracle-${tree.name}`) : members[importer],
          version: "1.0.0",
          private: true,
          dependencies,
        },
        null,
        2,
      ),
    );
  }

  if (Object.keys(members).length > 0) {
    writeFileSync(
      path.join(dir, "pnpm-workspace.yaml"),
      `packages:\n${Object.keys(members)
        .map((at) => `  - ${at}\n`)
        .join("")}`,
    );
  }

  pnpm(["install", "--lockfile-only", "--ignore-scripts"], dir);
  return yaml.load(readFileSync(path.join(dir, "pnpm-lock.yaml"), "utf8"));
}

/// pnpm's answer, with its key spelling thrown away.
///
/// jerky spells a peer context differently on purpose, so a comparison over
/// rendered keys would report a difference in spelling as a difference in
/// resolution. Nodes are numbered instead and every edge names a number, which
/// leaves a structure both implementations can be asked to produce.
function recordPnpm(lock, tree) {
  const snapshots = lock.snapshots ?? {};
  const keys = Object.keys(snapshots).sort();
  const id = new Map(keys.map((key, index) => [key, index]));

  // A snapshot's dependency value is the rest of a key: `18.3.1`, or
  // `18.3.1(react@18.3.1)` where the dependency itself carries a context.
  const target = (name, value) => {
    const key = `${name}@${value}`;
    if (!id.has(key)) throw new Error(`${tree.name}: edge to \`${key}\`, which the lockfile omits`);
    return id.get(key);
  };

  const nodes = [];
  for (const key of keys) {
    const snapshot = snapshots[key] ?? {};
    const base = published(key);
    const [name, version] = split(base);
    const resolved = { ...(snapshot.dependencies ?? {}), ...(snapshot.optionalDependencies ?? {}) };

    // pnpm merges a resolved peer into the snapshot's dependencies, so which
    // entries are peers is a question only the published manifest answers.
    const declaredPeers = Object.keys((lock.packages ?? {})[base]?.peerDependencies ?? {});

    const dependencies = {};
    for (const dependency of Object.keys(metadataCache.get(base).dependencies ?? {})) {
      const value = resolved[dependency];
      if (value === undefined) {
        throw new Error(`${tree.name}: ${key} declares \`${dependency}\` but pnpm resolved nothing`);
      }
      dependencies[dependency] = target(dependency, value);
    }

    const peers = {};
    for (const peer of declaredPeers) {
      // Absent is the answer for an unsatisfied peer, optional or not, and it
      // has to stay absent rather than become a null — the phenomenon under
      // test is the edge that does not exist.
      if (resolved[peer] !== undefined) peers[peer] = target(peer, resolved[peer]);
    }

    nodes.push({
      id: id.get(key),
      name,
      version,
      dependencies: sorted(Object.entries(dependencies)),
      peers: sorted(Object.entries(peers)),
    });
  }

  const importers = {};
  for (const at of Object.keys(tree.importers)) {
    const links = {};
    const section = (lock.importers ?? {})[at]?.dependencies ?? {};
    for (const [name, entry] of Object.entries(section)) {
      links[name] = target(name, entry.version);
    }
    importers[at] = sorted(Object.entries(links));
  }

  return { nodes, importers };
}

/// Every published version the tree reached, with the ranges that make it
/// resolvable again offline.
///
/// Only the versions pnpm selected are recorded. Version *selection* is the
/// semver oracle's subject, and holding the candidate set to pnpm's answer is
/// what leaves peer resolution as the only thing this oracle can disagree
/// about.
async function recordPackages(lock, tree) {
  const bases = [...new Set(Object.keys(lock.snapshots ?? {}).map(published))].sort();
  const packages = [];

  for (const base of bases) {
    const [name, version] = split(base);
    const metadata = await manifest(base);
    const peerRanges = metadata.peerDependencies ?? {};
    const peerMeta = metadata.peerDependenciesMeta ?? {};

    packages.push({
      name,
      version,
      // `optionalDependencies` are deliberately absent: jerky does not
      // implement them yet, and a platform-gated one would make the recording
      // depend on the machine that took it.
      dependencies: sorted(Object.entries(metadata.dependencies ?? {})),
      peers: sorted(
        Object.entries(peerRanges).map(([peer, range]) => [
          peer,
          { range, optional: peerMeta[peer]?.optional === true },
        ]),
      ),
    });
  }

  for (const entry of packages) {
    for (const dependency of Object.keys(entry.dependencies)) {
      if (!bases.some((base) => split(base)[0] === dependency)) {
        throw new Error(
          `${tree.name}: ${entry.name}@${entry.version} depends on \`${dependency}\`, which the recording has no version of`,
        );
      }
    }
  }

  return packages;
}

async function main() {
  const work = mkdtempSync(path.join(tmpdir(), "jerky-peer-oracle-"));
  try {
    // js-yaml rather than a hand-rolled parser: a subtly mis-read lockfile
    // would produce a fixture that looks plausible and asserts the wrong
    // thing, which is worse than no oracle at all.
    const tools = path.join(work, "tools");
    mkdirSync(tools, { recursive: true });
    writeFileSync(
      path.join(tools, "package.json"),
      JSON.stringify({ name: "oracle-tools", private: true, dependencies: { "js-yaml": JS_YAML } }),
    );
    pnpm(["install", "--ignore-scripts"], tools);
    const yaml = createRequire(path.join(tools, "index.mjs"))("js-yaml");

    const version = execFileSync("corepack", [PNPM, "--version"], {
      env: { ...process.env, COREPACK_ENABLE_DOWNLOAD_PROMPT: "0" },
    })
      .toString()
      .trim();

    const trees = [];
    for (const tree of TREES) {
      const lock = resolveWithPnpm(tree, work, yaml);
      const packages = await recordPackages(lock, tree);
      const pnpmAnswer = recordPnpm(lock, tree);

      process.stderr.write(
        `${tree.name}: ${packages.length} published, ${pnpmAnswer.nodes.length} nodes\n`,
      );

      trees.push({
        name: tree.name,
        phenomenon: tree.phenomenon,
        importers: sorted(Object.entries(tree.importers)),
        members: sorted(Object.entries(tree.members ?? {}).map(([at, name]) => [name, at])),
        packages,
        pnpm: pnpmAnswer,
      });
    }

    writeFileSync(
      OUT,
      `${JSON.stringify(
        {
          oracle: `pnpm ${version}`,
          generator: "tests/fixtures/generate-pnpm-peer-oracle.mjs",
          settings: NPMRC.trim().split("\n"),
          trees,
        },
        null,
        1,
      )}\n`,
    );
    process.stderr.write(`wrote ${OUT}\n`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

await main();
