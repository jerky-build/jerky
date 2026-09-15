# Benchmarks

`./benches/bench.sh` times `jerky install` against two real dependency graphs.
Until now jerky's only measured tree was `express@5.2.1` — 69 packages — which
is small enough that several costs this project has already made decisions
about cannot show up in it at all.

```
./benches/bench.sh                          # both fixtures, jerky only
./benches/bench.sh --fixture alotta-files   # one of them
./benches/bench.sh --npm --trials 5         # npm alongside, five trials
./benches/bench.sh --pin                    # re-resolve and rewrite the pins
```

It prints a markdown table per fixture, so a result can be pasted into an issue
as it stands.

## The two fixtures

Both come from [pnpm/benchmarks](https://github.com/pnpm/benchmarks), the
fixtures behind the numbers published at pnpm.io/benchmarks. Their
`package.json` files are **vendored verbatim** under `benches/fixtures/`, copied
at upstream commit `59fab2eb`. Vendored rather than fetched so the benchmark
runs offline and so an upstream edit cannot silently change what a number
means — pnpm's own repo warns about exactly this, and the runs recorded before
such a change have to be thrown away.

`alotta-files` is a 2018-era React/webpack/gulp/babel app: 109 dependencies and
one devDependency, carets throughout. (Its `name` field says `alotta-modules`;
that is upstream's, left alone.) `alotta-packages` is fourteen exact-pinned CLI
toolchains — `@angular/cli`, `@nestjs/cli`, `@vue/cli-service`, `eslint`,
`gatsby`, `grunt`, `gulp`, `jest`, `nuxt`, `react-scripts`, `serverless`,
`storybook`, `vite`, `webpack`.

**They are not redundant, and size is not why the second one exists.**

| | `express@5.2.1` | `alotta-files` | `alotta-packages` |
|---|---|---|---|
| direct dependencies | 1 | 110 | 14 |
| packages installed | 69 | 1291 | 2910 |
| scoped names | 0 | 57 | **538** |
| names at more than one version | 0 | 147 | **464** |
| levels in the walk | 7 | 7 | **10** |
| files on disk | ~600 | ~40k | **93k** |

These shape figures are **quoted from issue #72**, not produced by this
harness — it measures time, not graph shape, and only the package counts and
the link totals in its footer are regenerated on every run. The package counts
are the ones to check a change against: `--pin` reproduced 1291 and 2910 here,
which is what says the pinned graphs still match what the issue recorded.

Nine times the scoped names and three times the duplicate-version names, on
2.2x the packages: `alotta-packages` is the only fixture here that exercises
the virtual store's whole reason for existing at any scale. It is also three
levels deeper, which is what the level-synchronous resolver walk is paced by.

pnpm's third fixture, `bit-cli`, is **out of scope**: it depends on `@teambit/*`
and `@bitdev/*` from a registry of Bit's own, and reaching them means putting a
router in front of two registries. jerky talks to one registry and has no
per-scope registry configuration, so the fixture is not installable here at all.

## The four scenarios

The same four the `express` table used, so express and both fixtures compose
into one picture rather than three.

| scenario | what it is |
|---|---|
| cold store, no lockfile | a first install on a new machine |
| warm store, no lockfile | the re-resolution an edited `package.json` forces — the common developer install |
| warm store + lockfile | CI, and a fresh clone |
| no-op, tree already present | |

"Warm" means everything jerky keeps under `$HOME`: the content store **and**
the metadata cache. Both are filled by the cold run that precedes the warm
rows, which is what makes the second row measure a real re-resolution rather
than a first install — and, since the metadata cache landed, what makes it
fast.

Each is a median over `--trials` runs (3 by default). Medians rather than
means, because one run that lost a connection and retried moves a mean and does
not move a median, and a benchmark against a live registry gets one of those
regularly.

## How a run is isolated

- **`HOME` points at a fresh scratch directory**, so the store being measured is
  never the developer's own and "cold" is genuinely cold. Deleting `~/.jerky`
  to take a cold number is never necessary.
- **The vendored fixtures are copied out, never installed into.** Nothing under
  `benches/fixtures/` is written by a measuring run; only `--pin` writes there.
- **The scratch directory is removed on exit**, including on failure.
- **A release binary**, because a debug `jerky` measures the optimiser rather
  than the install. Set `JERKY_BIN` to measure a binary built somewhere else —
  a previous commit, which is most of what a benchmark is for.
- **A millisecond clock**, checked before the first measurement rather than
  found missing halfway through one. bash 5's `EPOCHREALTIME` is preferred
  because it costs no subprocess, which matters when the thing being timed is a
  30ms no-op; GNU `date +%s%N` and coreutils' `gdate` are the fallbacks. macOS
  ships neither bash 5 nor GNU date, so on a stock macOS this wants
  `brew install bash` or `brew install coreutils` — the harness says so and
  stops. The helpers themselves are written for bash 3.2 and are exercised on
  both runners in premerge.

Every run reports the packages installed and the dangling links out of the
total, because a timing taken against a tree `require` cannot walk is measuring
something other than a working install.

## The pins

`alotta-files` is carets all the way down and `alotta-packages` pins only its
fourteen direct dependencies, so both graphs drift under us: a number taken
today is not comparable to one taken next month. The fix is the committed
`jerky-lock.json` beside each fixture's manifest. jerky pins exact by default,
so a lockfile per fixture is both the pin and a thing worth having.

The two lockfiles are large — 466KB for `alotta-files`, 1.1MB for
`alotta-packages` — because they record 1291 and 2910 packages each. That is
the cost of the pin, and it is paid once per re-pin rather than per run.

`--pin` re-resolves both fixtures and rewrites those lockfiles. It is
deliberately a separate command and a separate, reviewable commit: **moving the
pins invalidates every number taken before the move**, and that should be
visible in a diff rather than happening inside a benchmark run.

The two lockfile-bearing scenarios copy the committed lockfile in; the two
without it start from the manifest alone. The pin is therefore what the
`warm store + lockfile` and `no-op` rows reproduce, and what the other two rows
converge on.

## Comparing against npm

`--npm` adds an npm column, and prints the following caveat **under every table
that has one** rather than once at the end of a run — a reader pastes one
fixture's table into an issue, and it has to travel with it. Three things:

- **The two columns do not count the same tree.** jerky does not resolve peer
  dependencies yet, so it installs fewer packages than npm does from the same
  manifest — 1291 against 1460 on `alotta-files`, 2910 against 3870 on
  `alotta-packages`. The gap is wider on the toolchain fixture, because peer
  dependencies pull in more of a toolchain graph than of an application graph.
- **npm refuses both fixtures outright without `--legacy-peer-deps`**, which
  the harness passes. That is not a thumb on the scale; it is the only way npm
  installs these manifests at all.
- **npm writes more files than jerky for the same fixture** — 110k against 93k
  on `alotta-packages` — because a hoisted tree duplicates what an isolated
  store shares.

npm's scenarios are the nearest equivalents rather than identical ones: npm has
no store separate from its cache, and `npm install` always writes a
`package-lock.json`, so "no lockfile" means removing the one the previous run
wrote and "warm" refers to the cache. npm's project directory is kept separate
from jerky's for that reason — a stray `package-lock.json` in jerky's project
would quietly change what the no-lockfile rows measure.

pnpm is not measured. It is the obvious third column and the harness has no
support for it yet.

## Tests

`./benches/lib-test.sh` covers the helpers in `benches/lib.sh` — the median,
the formatting, and the link counting. The benchmark itself cannot be tested,
since it measures a live registry, but the arithmetic under it is where a
silently wrong number would come from.
