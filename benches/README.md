# Benchmarks

`./benches/bench.sh` times `jerky install` against two real dependency graphs.
Until now jerky's only measured tree was `express@5.2.1` — 69 packages — which
is small enough that several costs this project has already made decisions
about cannot show up in it at all.

```
./benches/bench.sh --record                 # seed the replay mirror (once)
./benches/bench.sh --record --pnpm          # and what pnpm asks for as well
./benches/bench.sh                          # both fixtures, jerky only
./benches/bench.sh --fixture alotta-files   # one of them
./benches/bench.sh --npm --trials 5         # npm alongside, five trials
./benches/bench.sh --pnpm                   # pnpm alongside, same mirror
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

**`--record --pnpm` records what pnpm asks for too**, by driving a pnpm install
per fixture through the same proxy. It has to be asked for, because the two
tools do not want the same packages: pnpm installs peer dependencies and jerky
does not yet ([#34](https://github.com/jerky-build/jerky/issues/34)), so a
recording taken for jerky alone 404s at the first peer pnpm reaches. On
`alotta-files` that is 49 packuments and 65 tarballs pnpm needs and jerky never
asked for — 1146 and 1356 against jerky's 1097 and 1291, 15MB on top of 129MB.

One pnpm install per fixture covers all four pnpm rows. The lockfile rows
install from a `pnpm-lock.yaml` pnpm generates from the same manifest against
the same frozen recording, so they cannot want anything the manifest install
did not already ask for.

The recording records which tool it covers, per fixture, as an empty
`covers-pnpm-<fixture>` file beside the packuments. `--pnpm` checks for it
before it times anything and names the command that takes what is missing,
rather than letting a 404 arrive four minutes into a scenario and read as a
broken benchmark. A recording taken before pnpm was measurable has no such
file, which is the right answer for it.

The mirror itself needed no changes. It ignores the client's `Accept` and
serves the abbreviated packument it recorded, so pnpm's content negotiation
lands on the same document jerky's does; the serve-time `dist.tarball` rewrite
is on the response rather than on the client, so pnpm's tarball requests are
redirected at the mirror exactly as jerky's are.

**Do not commit it.** A full recording is roughly **770MB**, measured rather
than estimated: `alotta-files` is 1,097 packuments and 1,291 tarballs at
**129MB**, and `alotta-packages` is 2,238 packuments and 2,916 tarballs at
**643MB** — five times `alotta-files`, not the 2.2x its package count
suggests, because its packages are larger as well as more numerous. That is
not a thing to put in a git history that also has to be cloned. It lands in
`benches/.mirror/`, which is gitignored; `JERKY_BENCH_MIRROR_DIR` moves it
elsewhere. A measuring run with no recording to replay stops before it times
anything and says which command to run.

Re-pinning and re-recording go together, in that order: `--pin` resolves
against the live registry by definition, and a mirror recorded before it is a
recording of the old pin.

### Tarball URLs

`dist.tarball` in a packument is an **absolute** URL at the upstream registry,
so something has to redirect it at the mirror. There were two places to do it,
and this takes the second: **the mirror rewrites packuments as it serves
them**, replacing the upstream origin with its own.

The alternative to rewriting at serve time is rewriting at record time, which
bakes a host and a port into 770MB of data — a recording that could not be
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
  under every one of them. **`--pnpm` does not**: pnpm replays the same
  recording jerky does, which is what makes that column two times worth reading
  against each other rather than a shape comparison.

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

**Every column here runs with lifecycle scripts off**, npm's included. jerky
runs none at all — there is no `Command` anywhere in `src/` — so a column that
ran them would be timing an install plus a node-gyp build against one that was
not. One convention across the whole table, rather than a different fairness
rule per column, and both caveats say so.

## Comparing against pnpm

`--pnpm` adds a pnpm column, **against the same replay mirror as jerky's**. So
unlike the npm column, neither side carries the network and the two numbers are
comparable as times rather than only as shapes.

### Pointing pnpm at the mirror takes more than `registry=`

A `registry=` line in a project `.npmrc` is the first half, and on its own it
is **not** enough. npm-family configuration is per scope, and a more specific
key wins wherever it was written: `registry=` does not override a
`@scope:registry=` line configured elsewhere. Both fixtures are full of scoped
names — `alotta-packages` is `@angular/cli`, `@nestjs/cli`, `@vue/cli-service`
and eleven more, and `alotta-files` reaches dozens of transitive `@babel/*` —
so a scope pointed somewhere else is a measured install going somewhere else.

That failure is the dangerous kind. The mirror answers a miss with a loud 404,
so an unscoped leak stops the run; a scoped one never reaches the mirror at
all, and is silent, live, and inside a number this harness promises is offline.

Three things close it, and it takes all three:

- **`HOME` already points at the scratch directory**, so `~/.npmrc` is not
  read. That is the layer people think of first, and it was never the problem.
- **`userconfig` and `globalconfig` are pointed at an empty file** for every
  pnpm invocation. The global npmrc lives with the node installation rather
  than under `$HOME`, so redirecting `HOME` does not cover it.
- **Whatever scope-specific registry still survives is re-pointed at the
  mirror.** Emptying the files is not sufficient: pnpm ships `@jsr:registry` as
  a built-in default, which no file can unset. So the harness asks pnpm what
  scoped registries it would use, writes an override for each into the project
  `.npmrc`, and then asks again — refusing to measure if any of them still
  names something other than the mirror. Asked of pnpm rather than derived from
  the fixture's manifest, because a scope can be configured that no manifest
  mentions, and because the built-in is in none of them.

### What the column says about itself

It prints the following under every table that has the column, for the reason
the npm caveat does: a reader pastes one fixture's table into an issue.

- **jerky and pnpm do not count the same tree**, for the same reason jerky and
  npm do not. jerky resolves no peer dependencies yet
  ([#34](https://github.com/jerky-build/jerky/issues/34)), so pnpm installs
  packages jerky never asks for.
- **pnpm's scenarios are the nearest equivalents rather than identical ones.**
- **Lifecycle scripts are off in every column**, which is the npm section's
  point above and applies here unchanged.

The mapping, which is where "nearest equivalent" is decided:

| scenario | what pnpm does |
|---|---|
| cold store, no lockfile | pnpm's content-addressable store **and** its metadata cache both wiped. Those are the two things jerky's cold row wipes when it deletes `$HOME`, so "warm" means the same pair on both sides. |
| warm store, no lockfile | both inherited from the cold row, project started from the manifest alone |
| warm store + lockfile | installed **frozen** from a `pnpm-lock.yaml` pnpm generated itself in untimed setup |
| no-op, tree already present | the same, run once untimed first so the timed run finds the tree already there |

Two of those want a word. Like npm, **pnpm writes a lockfile on every
install**, so "no lockfile" is a property of the directory a run starts in
rather than a flag — and pnpm's project directory is kept apart from jerky's so
a stray `pnpm-lock.yaml` cannot change what jerky's no-lockfile rows measure.
And **the lockfile rows install from a pin pnpm made**, not from a committed
one: there is no `pnpm-lock.yaml` beside the fixtures the way there is a
`jerky-lock.json`, and there should not be, since what pnpm's CI row installs
is a lockfile pnpm wrote. It is generated in untimed setup rather than
inherited from whichever scenario ran last, so the row cannot quietly measure a
no-lockfile install the first time it runs.

**Frozen is decided per scenario, not once for the run.** It is on for the two
lockfile-bearing rows and off for the two without, where there is no lockfile
to freeze and frozen is an error. Setting it off everywhere — which is what
this did at first — leaves pnpm re-checking the lockfile against
`package.json` on the two rows that have one, which is work jerky's lockfile
row does not do, in the two rows jerky already wins. Frozen is also what pnpm
turns on by itself when `CI` is set, and CI is what those rows are named after.

On `alotta-files` the correction is small — 0.87s frozen against 0.89s not, and
no measurable difference on the no-op row — because `prefer-frozen-lockfile`
already defaults to true and takes the headless path when the lockfile happens
to be up to date. What the explicit flag buys is that the row no longer depends
on that happening to hold: non-frozen *falls back* to a full re-resolution when
anything mismatches, which would turn the CI row into a resolution benchmark
without saying so, where frozen makes the same mismatch an error.

pnpm's store, cache and state directories are moved into the scratch tree
rather than left under `$HOME`. That is not tidiness: the cold *jerky* trial
deletes `$HOME` outright, so a pnpm store under it would be wiped by the other
column's setup and every pnpm row would measure a cold install.

pnpm is not vendored. `--pnpm` checks for it on `PATH` alongside the checks for
a millisecond clock and for `python3`, so a missing pnpm is a sentence rather
than half a table. All of those now run **before** the release build rather
than after it, which they did not at first: a mistyped flag used to cost a full
`cargo build --release` before anything told you about it.

## Tests

`./benches/lib-test.sh` covers the helpers in `benches/lib.sh` — the median,
the formatting, the table, the link counting, and the mirror's lifecycle and
what it serves. The timing loop itself is still untested: what it does is run a
stopwatch around a subprocess, and the parts of it that could be silently wrong
are the arithmetic and the mirror, which are both here.

The table is built from a list of column names rather than as a literal per
combination of flags, and that is what the header and row tests are protecting.
jerky's column is always there and npm's and pnpm's are each optional, so the
literal form would be four headers to keep in agreement with four rows; the
pair that drifts prints a separator with the wrong number of cells, which
markdown renders as a table with a column quietly missing off the end of the
numbers.

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
