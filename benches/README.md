# Benchmarks

`./benches/bench.sh` times `jerky install` against two real dependency graphs.
Until now jerky's only measured tree was `express@5.2.1` — 69 packages — which
is small enough that several costs this project has already made decisions
about cannot show up in it at all.

```
./benches/bench.sh --record                 # seed the replay mirror (once)
./benches/bench.sh                          # both fixtures, jerky only
./benches/bench.sh --fixture alotta-files   # one of them
./benches/bench.sh --npm --trials 5         # npm alongside, five trials
./benches/bench.sh --pin                    # re-resolve and rewrite the pins
```

It prints a markdown table per fixture, so a result can be pasted into an issue
as it stands.

**A measuring run makes no request at the live registry.** It replays a
recording from disk; `--record` is what takes one, and it has to be run once
before the first benchmark. See [The replay mirror](#the-replay-mirror).

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
not move a median. That used to happen regularly, when every trial went to the
live registry; the replay mirror has removed the cause rather than the
precaution, and a median is still the right summary of three runs on a machine
that is also doing other things.

## How a run is isolated

- **The registry is a local replay mirror**, so a measuring run makes no
  request at `registry.npmjs.org` at all. Its own section is below.
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

## The replay mirror

A default run used to make roughly **22,600 anonymous requests** at
`registry.npmjs.org`. The cold scenario wipes the scratch `HOME` before every
trial, so each one refetches everything: 2,238 packuments and 2,910 tarballs
for `alotta-packages` alone, times three trials, times two fixtures. That is a
rate limit waiting to happen, and it is also why the numbers could not be
reproduced — a retry after a lost connection is weather, and folding it into a
median makes the median a measurement of the day rather than of the code.

So the benchmark serves the registry itself. `benches/mirror.py` replays
recorded packuments and tarballs over loopback, `bench.sh` starts it, points
jerky at it and stops it on the way out, and the four scenarios are otherwise
unchanged.

### The environment override

jerky reads `JERKY_REGISTRY_URL` and talks to that registry instead of npm's.
The `RegistryClient` seam and the base-URL override have been there since the
client was written — around twenty integration tests drive it — and the only
missing link was that the binary hardcoded the default at its composition
point. `main` reads the variable, which is the same rule `$HOME` follows:
nothing below `main` reads the environment.

It is a user-visible variable rather than a benchmark-only one, and it is in
the CHANGELOG as such. An empty value means the same as an unset one.

**This is not an offline mode.** Offline mode is
[#80](https://github.com/jerky-build/jerky/issues/80) and is a feature for
users — a decision about what jerky does when the registry cannot be reached.
This is bench tooling hung on a seam that already existed: pointing jerky at a
different HTTP registry leaves every failure mode exactly as it was.

### Taking a recording

```
./benches/bench.sh --record
```

**A separate, deliberate step, for the same reason `--pin` is.** Re-recording
invalidates every number taken before it, so it belongs in a diff rather than
inside a measuring run — and since the recording itself is not committed, the
diff it belongs in is the one that says the numbers were retaken.

`--record` starts the mirror as a caching proxy and drives real installs
through it: whatever jerky asks for is fetched from the live registry, written
down verbatim, and served. Recording *by observation* rather than from a
package list is what makes a recording complete by construction. A list built
from the two committed `jerky-lock.json` files would look equivalent and is
not — the two no-lockfile scenarios re-resolve from the manifest, so a caret
range that has picked up a newer version since the pin was taken wants a
tarball no pin names. Two installs per fixture cover both: one from the
manifest alone, one from the pin.

It resumes. An interrupted recording is finished by running it again, since
anything already on disk is a hit.

**Do not commit it.** A full recording is roughly **470MB**. `alotta-files`
alone measures 1,097 packuments and 1,291 tarballs at **129MB**, and
`alotta-packages` is 2.2x its packages; that is not a thing to put in a git
history that also has to be cloned. It lands in `benches/.mirror/`, which
is gitignored; `JERKY_BENCH_MIRROR_DIR` moves it elsewhere. A measuring run
with no recording to replay stops before it times anything and says which
command to run.

Re-pinning and re-recording go together, in that order: `--pin` resolves
against the live registry by definition, and a mirror recorded before it is a
recording of the old pin.

### Tarball URLs

`dist.tarball` in a packument is an **absolute** URL at the upstream registry,
so something has to redirect it at the mirror. There were two places to do it,
and this takes the second: **the mirror rewrites packuments as it serves
them**, replacing the upstream origin with its own.

The alternative to rewriting at serve time is rewriting at record time, which
bakes a host and a port into 470MB of data — a recording that could not be
replayed on a different port, let alone moved between machines. The mirror
binds a kernel-chosen port precisely so two benchmarks on one machine cannot
collide. The other alternative, teaching the client to resolve `dist.tarball`
against its base URL, changes `RegistryClient`'s contract in shipped code for
the benefit of a benchmark, and would quietly change what jerky does with a
registry that legitimately serves tarballs from another host.

Tarball *bytes* are never touched, so integrity verification is exactly as real
here as it is against npm. The rewrite is a substitution of one origin string,
which is all the npm registry needs; a packument naming a tarball anywhere else
is reported at record time, where nothing is being timed.

**The pins need the same treatment, and that is easy to miss.** A lockfile
records `resolved` as an absolute URL as well, and the two lockfile-bearing
scenarios install straight from it *without fetching a packument at all* — so
nothing the mirror does while serving one can redirect them. `bench.sh` copies
each pin into the scratch directory with its URLs pointed at the mirror and
installs from the copy; the committed `jerky-lock.json` keeps naming the
registry that actually answered, which is the point of a pin. Without this the
mirror sits there serving nothing while every tarball request goes to npm — and
that is not hypothetical, it is what the first run against the mirror did, in a
network namespace with no route off the machine, which is how it was caught
rather than shipped.

### What it costs to read this way

Two things a reader of the numbers should know:

- **The mirror is in the measurement.** Localhost instead of the internet makes
  the cold rows much faster than the live-registry figures recorded before this
  existed, so numbers from either side of this change are not comparable. What
  the mirror buys is that numbers taken *after* it are comparable with each
  other, which is what a benchmark is for.
- **`--npm` still fetches live.** npm is not pointed at the mirror — it wants
  endpoints and a packument form the recording does not hold — so an `--npm`
  run is the one mode here that reaches the live registry while measuring, and
  its column carries the network where jerky's does not. The tables say so
  under every one of them.

`mirror.py` wants `python3` on `PATH`, and `bench.sh` says so and stops rather
than discovering it halfway through a run.

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
the formatting, the link counting, and the mirror's lifecycle and what it
serves. The timing loop itself is still untested: what it does is run a
stopwatch around a subprocess, and the parts of it that could be silently wrong
are the arithmetic and the mirror, which are both here.

The mirror's tests run against a synthetic recording of one package, generated
by the test, so they need neither a real recording nor a network. The rewrite
is the part worth testing hardest: a mirror that served `dist.tarball`
verbatim would send every tarball request straight back to the live registry,
and a benchmark that looked like it was replaying and was not is worse than one
that obviously is not.

That the *binary* honours `JERKY_REGISTRY_URL` is a Rust test —
`the_registry_comes_from_the_environment` in `tests/main_test.rs` — which
installs a package from a throwaway HTTP server addressed through the variable.
A full install rather than a probe, because what has to be true is that the
packument and the tarball both land on the override.
