# Changelog

Notable user-visible changes. Anything that alters what jerky prints, writes,
or refuses belongs here; internal refactors do not.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- A package's binaries are linked into `node_modules/.bin`, so a
  locally-installed tool actually runs. `jerky install typescript` now leaves
  you a `tsc` you can execute, where before it left a `node_modules` in which
  nothing was runnable at all. Bins are read off the abbreviated packument —
  the same response dependencies and `os` already come from, so no extra
  request — and both shapes npm documents are accepted, the object and the bare
  string that takes the package's own name.

  **Direct dependencies only**, matching npm and pnpm: a transitive
  dependency's CLI is not something the project declared. Each package also
  gets its dependencies' bins in its *own* private `.bin` inside the virtual
  store, because with no ambient hoisting there is nowhere else for a package
  that shells out to a dependency's CLI to look. Workspace members publish
  their bins to the members that depend on them, read from their own
  `package.json` since a member has no packument.

  **A bin that ships non-executable is made executable.** npm packages
  routinely publish their `bin` target at `0o644` and rely on the installer to
  fix it. jerky raises the execute bits — `| 0o111`, never a mode replacement —
  on the target itself, which because the virtual store is hard links means on
  the shared store entry too. That is deliberate and argued in
  `docs/research/2026-09-14-pnpm-bin-executability.md`: everyone sharing a
  jerky store entry has the same package at the same version and so declares
  the same bins. The one exception is a **workspace member's own file**, which
  is never chmodded — it is in your repository under version control, where a
  raised execute bit is a change `git status` reports.

  **A `bin` jerky will not spell is dropped rather than fatal.** `bin` is
  published by whoever published the package and both halves of every entry
  become a path, so a name that is not a single path component and a target
  that climbs out of the package are refused — the rest of that package's bins
  install normally.

  **Two dependencies publishing one name** is resolved in favour of the
  alphabetically first, so the tree is the same on every machine, and jerky
  says which one got the name rather than leaving you to find out:

  ```
  warning: alpha and zeta both publish a `fmt` binary — …/.bin links alpha's
  ```

  Shims are converged like every other link: one whose dependency left the
  manifest is removed, along with a `.bin` jerky empties, while a shim npm or
  yarn wrote for a package you no longer depend on is reported and left where
  it was found.

  **Migrating from yarn classic needs a clean `node_modules`.** yarn writes
  shell scripts into `.bin` where npm and jerky write symlinks, and jerky will
  not overwrite a file it cannot prove it wrote — so an install that has to put
  a shim exactly where one of those scripts sits stops and names it rather than
  deleting it. `rm -rf node_modules` and install again
  ([#20](https://github.com/jerky-build/jerky/issues/20)).

  **If you have a lockfile from before this change**, it records no `bin` for
  anything, and nothing distinguishes that from a package that declares none —
  so no shims are written until the project re-resolves. Touch a `package.json`
  or delete `jerky-lock.json` to pick them up. Pre-1.0, on-disk formats change
  in place rather than carrying a migration.

- `optionalDependencies` are resolved, and the ones this machine cannot run are
  skipped. A package declaring an `os` or `cpu` that rules out the machine —
  every `@esbuild/*`, `@rollup/rollup-*` and `@napi-rs/*` platform variant, and
  `fsevents` — is left out of the tree when it was reached through an
  `optionalDependencies` entry, along with anything only it led to. jerky says
  so in one line rather than one per package, because a project using esbuild,
  rollup and swc skips some seventy of them on every install:

  ```
  skipped 23 optional packages unsupported on linux-x64
  ```

  **A platform mismatch is the only thing that skips.** A fetch error, an
  integrity mismatch and an unpack failure are fatal on an optional dependency
  exactly as they are on a required one. npm tolerates all four, because
  `optionalDependencies` was added for native addons whose `node-gyp` build
  fails — and jerky runs no lifecycle scripts, so that failure cannot happen
  here at all. What is left is not failure tolerance but a package saying in
  advance which machines it is for. Reading `optional: true` as a waiver on the
  integrity check in particular would let anyone who can serve bad bytes for
  `fsevents` remove `fsevents` from your tree with no output at all.

  **The lockfile is the same on every platform.** Every optional dependency is
  resolved and recorded whatever machine writes the file, with the `os` and
  `cpu` it declared, so a lockfile committed from a Mac installs on Linux
  without reading as stale — and a `--production` install plans the right tree
  for the machine it is running on from that one file, resolving nothing
  ([#107](https://github.com/jerky-build/jerky/issues/107)).

- Registry metadata is cached on disk, in `~/.jerky/cache` beside the store.
  A packument answers from there for **24 hours** without reaching the
  registry at all, so re-resolving a manifest you just edited — the most
  common install there is — now does no network work for metadata. Measured on
  a 2910-package tree: the metadata walk was ~9s of round trips, and inside the
  window it is none. Past 24 hours an entry is *revalidated* rather than
  re-downloaded: jerky sends the stored `ETag`, and a `304` re-stamps the entry
  as current without moving a byte of body, which on that tree is 0.7 MB
  instead of 470 MB.

  **A dist-tag still asks every time.** `jerky install lodash`, `react@next` or
  a `"latest"` in a `package.json` reaches the registry however warm the cache
  is, because only the registry can say what a tag points at today — the
  promise below, kept. The request is conditional, so it is usually a `304`
  rather than a download.

  **A range is answered from the window, and that means it can miss a new
  release for up to a day.** `^4.0.0` resolved against a cached packument
  selects the newest version *that packument knew about*, so a version
  published an hour ago is invisible until the entry lapses. This is a
  deliberate trade and the reason it is acceptable is narrow: jerky pins exact
  by default and the lockfile records what was chosen, so a resolution taken
  from a stale entry is reproducible and shows up in a diff rather than
  drifting silently. If you need the newest release the moment it lands, ask
  for it by tag or by version — both reach the registry.

  **If the registry cannot be reached and the entry is past its window, the
  install stops** rather than resolving from it. The window is what bounds how
  stale an answer may be, and quietly serving a lapsed entry because the
  network happened to be down would remove that bound exactly when nobody is
  watching. The error says how old the cached copy is so you can decide what to
  do about it. A range inside the window makes no request at all, so that much
  resolves with no network; a dist-tag does not, because it always asks
  ([#70](https://github.com/jerky-build/jerky/issues/70)).

- Scoped packages install. `@types/node` and every `@babel/*`, `@eslint/*` and
  `@nodelib/*` a real tree pulls in now resolve, download and link, where
  before the install died on the first one with an `ENOENT` from the rename
  that puts a store entry in place. The virtual store nests them —
  `.jerky/@types/node@20.0.0` — which is the layout convergence and pruning
  were already written to expect, so both halves of the decision agree.
  Together with `npm:` aliases this is what a large real-world tree needed:
  `alotta-files`, the fixture the pnpm benchmarks use, installs its 1291
  packages in **11s** into a cold store, with every one of its 2502 symlinks
  resolving and `require` working through them. The scope directories jerky
  creates are `0o755` rather than whatever the umask allows, in an importer's
  tree and in the machine-global store alike
  ([#22](https://github.com/jerky-build/jerky/issues/22)).
- `jerky install` with no package installs what the workspace declares. This is
  the command you run after cloning a repo, and until now there was no way to
  say it: `spec` was a required argument, so jerky could add a dependency but
  not reproduce one. Every importer is covered, whatever directory you run it
  from, and with a lockfile that already matches the manifests it reaches the
  registry not at all. It reports what it did as `installed 12 packages across
  3 importers` — packages rather than links, because two importers on one
  version share a store entry and counting links would make the same install
  read differently in a monorepo
  ([#19](https://github.com/jerky-build/jerky/issues/19)).
- A bare `jerky install` works from a directory that belongs to no workspace
  member, such as `tools/scripts`, where `jerky install <pkg>` is still an
  error. Ambiguity needs alternatives, and a command that acts on every
  importer has nothing to guess.
- `devDependencies` are now installed. Every workspace member's are followed,
  not only the root project's — in a monorepo each member is a first-party
  project, and a `packages/ui` declaring nothing but devDependencies previously
  installed nothing at all. A dependency's own dev dependencies remain
  unreachable, as they must: following them pulls in most of the registry.
  The lockfile records which section asked, in a `devDependencies` block beside
  `dependencies` for each importer, so moving a package between the two shows
  up as a real diff and reinstalls rather than reading as no change. A project
  with no devDependencies gets no new block. `lockfileVersion` stays `1`; the
  format is unreleased ([#21](https://github.com/jerky-build/jerky/issues/21)).
  Recording a package *into* `devDependencies` from the command line is
  `--save-dev`, below; a name appearing in both sections is resolved as a
  production dependency, which is npm's answer, rather than refused — the
  manifest may not be the user's to edit.
- `jerky install --production` installs `dependencies` only, across every
  importer, and **requires a lockfile that already matches every manifest**.
  This is the CI form — jerky's `npm ci` — and it gives the frozen-lockfile
  story without a third flag to design. Requiring the match is what earns the
  read-only property rather than enforcing it: with every importer reusable
  there is nothing to resolve, so there is nothing to write, and the lockfile
  is left byte-identical. It performs no metadata requests at all; the only
  traffic is tarballs the store does not already hold. The match is over
  **both** sections, so editing a `devDependencies` entry without reinstalling
  fails even though no devDependency would have been linked — a mode that
  overlooked that would let CI pass on a lockfile that is genuinely out of
  date. A missing lockfile, or one that disagrees, is an error naming the
  dependency and both values, raised before anything is linked — as is a
  lockfile recording an importer the workspace no longer has, from a member
  dropped out of `workspaces` without reinstalling. Convergence is
  not selectively applied: a `node_modules` from an earlier ordinary install
  has its devDependency links *removed*. `--production` conflicts with a
  package argument and with `--save-dev`, rejected by clap at parse time
  ([#58](https://github.com/jerky-build/jerky/issues/58)).
- `jerky install --save-dev <pkg>`, or `-D <pkg>`, records the package under
  `devDependencies` rather than `dependencies`, in the workspace member you
  are standing in. It *moves* a package already declared in the other section
  rather than declaring it twice, and a section it empties by doing so is
  removed rather than left behind as `"dependencies": {}`. Without the flag the
  section is still whatever the manifest already says, so `jerky install
  lodash@4.18.0` remains a version change and never a promotion — and a
  manifest that declares the name in *both* sections keeps both, because
  settling that contradiction is not something a version change was asked to
  do. The lockfile
  is rewritten to agree, so the move is one diff rather than a manifest change
  the next install notices and repeats. `--save-dev` with no package is an
  error rather than a bare install that ignores the flag
  ([#57](https://github.com/jerky-build/jerky/issues/57)).
- The lockfile is now read back, not only written. An importer whose recorded
  specifiers still match its `package.json` is reused verbatim; one that does
  not is re-resolved. Staleness is per importer, so editing `apps/web` does not
  invalidate what the root already resolved. Repeating an install that named a
  version or a range — `jerky install lodash@4.17.21`, `jerky install
  lodash@^4.0.0` — then reaches the registry not at all. A bare `jerky install
  lodash` or a dist-tag still asks every time, because only the registry can
  say what `latest` means today; that is the request rather than a shortcoming
  ([#47](https://github.com/jerky-build/jerky/issues/47)).
- A lockfile entry's integrity hash is authoritative. If the registry later
  reports a different hash for a version the lockfile already pins, the install
  stops rather than proceeding — this is the trust-on-first-use anchor spec 1
  explicitly went without, and it is what makes the lockfile a security
  artifact rather than a cache. The comparison happens before the store is
  consulted: a store hit proves only that the bytes match their *own* hash,
  which says nothing about whether that hash is the one the lockfile pinned.
  A republished tarball whose bytes another project already placed in the
  machine-global store is exactly that case. Note that the check compares the
  hash algorithm as well as the digest, so an entry locked from a package's
  legacy sha1 `shasum` will report a mismatch if the registry later serves a
  sha512 `integrity` for it; the fix for now is to delete that lockfile entry
  and reinstall.
- `jerky install` now resolves the whole workspace in one walk and links every
  importer, rather than installing a single package into a single project.
  Members come from the root `package.json`'s `workspaces` field, which is
  what npm and yarn already read, so an existing monorepo needs no new file
  and no migration step ([#46](https://github.com/jerky-build/jerky/issues/46)).
- `jerky install <pkg>` operates on the importer you are standing in, found by
  walking up for the workspace root and then taking the nearest enclosing
  member. The command now reports which importer it installed into.
- Dependencies declared with the `workspace:` protocol — `workspace:*`,
  `workspace:^1.0.0` — resolve to the member of that name and are linked
  straight at its directory. The registry is never asked. Naming a package
  that is not in the repo is an error rather than a fall back to the registry,
  because it is a typo.
- `jerky-lock.json` is written at the workspace root, keyed by importer. There
  is one lockfile per workspace, not one per project
  ([#47](https://github.com/jerky-build/jerky/issues/47)).
- `JERKY_REGISTRY_URL` points jerky at a registry other than
  `https://registry.npmjs.org`. An empty value means the same as an unset one,
  so `JERKY_REGISTRY_URL= jerky install` is how a shell says "not the one my
  parent exported". The client could always take a base URL and the tests
  always drove it that way; the binary was what hardcoded the default, so
  nothing built out of `main` could be pointed anywhere else. This is not an
  offline mode ([#80](https://github.com/jerky-build/jerky/issues/80)) — a
  different registry is still a registry, and a missing one is still fatal.
  The benchmark is what wanted it: `./benches/bench.sh` now replays packuments
  and tarballs from a local recording instead of making ~22,600 anonymous
  requests at npm per run ([#83](https://github.com/jerky-build/jerky/issues/83)).

### Changed

- **An install that finds the tree already correct now writes nothing to it.**
  Placing a symlink used to remove and recreate it whether or not it already
  pointed where the install wanted, so a no-op install still paid one unlink
  and one symlink for every edge in the graph — every dependency of every
  package, plus every link in every importer's `node_modules`. Each link is
  now read first and left alone when it already says the right thing, which is
  also what stops a no-op install from moving the mtime of a tree nothing
  changed ([#86](https://github.com/jerky-build/jerky/issues/86)).

- **Packages are materialised into the virtual store in parallel.** Hard-linking
  a package out of the content store is syscall-bound rather than
  latency-bound, and the entries are independent of one another, so they now
  run across as many workers as the machine has cores — the cgroup's share of
  them inside a container — instead of one at a time. Linking the tree is
  roughly three to four times faster on a 32-core machine; the rest of the
  install is unchanged, and convergence and the prune deliberately stay on one
  thread, since what they do is *remove* files and their proof that jerky wrote
  what they are removing is not worth re-deriving under concurrency.

  Which package a failing install blames does not become a race: the worker
  pool reports the failure of the lowest-indexed item, which here is the first
  entry in the plan's own order, so the same broken package is named on every
  run.

  Laying out a tree of 2000 packages and ~90k files — the shape of the
  `alotta-packages` benchmark fixture — went from ~535 ms to ~120–215 ms on a
  32-core machine, and a second install over the finished tree from ~51 ms to
  ~20 ms. Those are the linker alone rather than a whole install
  ([#86](https://github.com/jerky-build/jerky/issues/86)).

- File permissions from a package tarball are no longer applied as recorded.
  Every extracted file becomes `0o644`, or `0o755` when the archive marked it
  owner-executable, and every directory becomes `0o755`; nothing else from the
  header survives. Directories jerky creates itself are covered
  too — the ones a tarball omits, the store entry's own root, and the ones the
  linker recreates inside a project — since those took their mode from the
  process umask and a permissive umask made them world-writable.

  A package can no longer put a group- or world-writable file or directory
  into the content store, which matters more there than elsewhere because the
  store is machine-global and hard-linked into every project: one set of
  permissions is shared by all of them at once, so anyone who can rewrite that
  file, or add an entry to that directory, changes what every project imports.

  The setuid and setgid bits were already dropped before this change, by the
  tar reader rather than by jerky; they are now jerky's own guarantee and a
  test fails if that stops being true
  ([#29](https://github.com/jerky-build/jerky/issues/29))
- `npm:` alias specifiers resolve. A dependency declared as
  `"width-cjs": "npm:string-width@^4.0.0"` installs `string-width` under the
  name its dependent calls it by, which is what `@isaacs/cliui` does and
  therefore what anything reaching a modern `glob` does — jerky previously
  reported such a specifier as a malformed version range and stopped. The
  package is stored and downloaded under its own name, so however many local
  names reach it there is one entry and one request. The lockfile records the
  real name beside the local one for a package's own dependencies as it
  already did for an importer's, and `jerky install width-cjs@npm:string-
  width@^4.0.0` records the whole `npm:` specifier in the manifest rather than
  the bare version — with the pin *inside* the scheme, so the default is still
  an exact version ([#73](https://github.com/jerky-build/jerky/issues/73)).
- A specifier jerky does not understand now says so. `file:`, `git:`,
  `github:` and anything else of the form `<scheme>:` are reported by name and
  as unsupported, rather than as a version range that failed to parse and then
  failed again as a dist-tag. None of them are supported; the difference is
  that the error now says which one you wrote and that the scheme is the
  problem.
- Packuments are fetched concurrently during resolution, up to sixteen at a
  time. The walk now proceeds a level at a time: it takes the whole frontier,
  fetches every packument that frontier will ask for at once, and then walks it
  exactly as before against a warm memo. Resolution discovers its own work, so
  unlike the tarball half this cannot be one flat work list — but a tree has
  few levels and wide ones, and express's 69 packages sit in seven, so seven
  rounds of requests replace sixty-eight. On that tree a cold install goes from
  3.00s to **1.04s**, and the re-resolution an edited `package.json` forces —
  warm store, stale lockfile, the common developer install — from 2.60s to
  **0.70s**. An install that reuses its lockfile is unaffected, because it
  asks the registry nothing ([#71](https://github.com/jerky-build/jerky/issues/71)).
- Tarballs are downloaded concurrently rather than one after another, up to
  sixteen at a time. Once the graph is settled every URL is known, so this half
  is a fixed work list with nothing to wait for — unlike resolution, which
  discovers its own work and is parallelised differently, above. On a cold store this is the
  difference between a first `jerky install` that reads as slow and one that
  does not; a repeat install is unaffected, because it already downloaded
  nothing. The cap exists so a large tree does not open a connection per
  package and get the machine rate-limited
  ([#33](https://github.com/jerky-build/jerky/issues/33)).
- A lockfile whose recorded integrity disagrees with the registry is now
  refused before *any* tarball is fetched, rather than after every package
  ahead of it in the graph had been. Serially "halfway through" at least had
  an order to it; with sixteen fetches in flight there is no ahead or behind,
  so the gate became a pass of its own. The check and its message are
  unchanged, but its *precedence* is not: a corrupt tarball on an early
  package used to be reported ahead of a locked-integrity mismatch on a later
  one, and now the mismatch always wins. That is the better answer of the two
  — a republished tarball is a claim about the lockfile, and it should not
  depend on where in the graph it landed.
- `jerky install` is now convergent rather than additive: after it runs, each
  importer's `node_modules` holds what its manifest declares and nothing else.
  A dependency you delete from a `package.json` loses its link on the next
  install, and its unpacked tree under `node_modules/.jerky` goes with it.
  Previously the link survived and still resolved, so `require` kept finding a
  package the project no longer declared
  ([#56](https://github.com/jerky-build/jerky/issues/56)).
- Only what jerky can prove it wrote is removed — a symlink pointing into this
  workspace's virtual store or at one of its members. A real directory left by
  a previous `npm install`, or a symlink into an `npm link` checkout, is left
  exactly where it is and reported as a warning naming the path. The first
  `jerky install` in a repository that has seen npm is not a destructive
  surprise. The machine-global content store under `~/.jerky/store` is never
  touched: it is shared by every project on the machine, so nothing
  project-local gets to decide one of its entries is dead.
- Two importers wanting the same version now share one store entry and one
  directory in the virtual store, which is why the store lives at the
  workspace root. Importers wanting different versions each get their own;
  agreement is deliberately not forced.
- A missing package is now reported through resolution rather than directly
  from the registry, since version selection is what asks.
- The registry client keeps a connection per worker rather than the HTTP
  library's three per host, so jerky's sixteen-way fan-out at a single registry
  stops paying a fresh TCP and TLS handshake for roughly thirteen of every
  sixteen requests. A cold install of a large tree made some 2,900 handshakes
  where it now makes tens. The churn that removes is also the pattern a
  registry rate-limits on, so the fix and the next entry are the same fix from
  two directions ([#82](https://github.com/jerky-build/jerky/issues/82)).
- A `429 Too Many Requests` is now an instruction rather than a transient
  fault. jerky waits the `Retry-After` the registry advertised, or a second and
  then two seconds when it advertised none, where before it retried after 100ms
  and 200ms exactly as it does for a 5xx — tripling traffic at the endpoint
  that had just asked for less. Still three attempts, so a rate limit that
  outlasts three seconds is reported rather than outwaited, and a `Retry-After`
  longer than a minute is reported straight away rather than slept off: it is
  more time than a command-line tool can sit on. This covers the revalidation
  the metadata cache does for a stale entry as much as a first fetch: a
  rate-limited revalidation is reported as one, where the only status that path
  reads as an answer is the `304` it asked for
  ([#82](https://github.com/jerky-build/jerky/issues/82)).
- **Resolution no longer waits for a whole level of the tree before starting
  the next one.** jerky used to fetch every packument one depth of the
  dependency graph wanted, wait for the slowest of them, and only then look at
  what they depended on — so a cold resolve cost the graph's depth times its
  slowest fetch per level, and `alotta-packages` is ten levels deep. A
  completed packument now schedules the packages it names immediately, so what
  a cold resolve waits for is the longest single chain of dependencies rather
  than the depth of the tree multiplied by its unluckiest fetch. The saving is
  a network one and shows up where round trips are slow and uneven: over the
  benchmark's local replay mirror, where a packument arrives in about a
  millisecond, it is inside the run-to-run noise
  ([#87](https://github.com/jerky-build/jerky/issues/87)).

  **It resolves to the same bytes.** Which version satisfies a range is still
  decided on one thread, in the order the walk has always taken, from a
  packument cache keyed by the *request* — the package **and** how current the
  answer had to be — so the order answers arrive in cannot reach the lockfile.
  Keying that cache on the package alone is not enough and is the mistake this
  entry was nearly shipped with: inside the metadata cache's window the two
  freshnesses are two different answers for one package, and a resolution that
  shared them would let one dependency's use of a dist-tag quietly change
  which version a completely different dependency's range resolved to. Six
  cold resolutions of `alotta-files` produce one byte-identical 1291-package
  lockfile, and it is the same file the previous resolver wrote.

  **The freshness rule is unchanged, and now costs one more request in one
  case.** A range may still be answered from the window and a dist-tag still
  reaches the registry every time. A package asked for *both* ways in one
  install — `jerky install lodash` in a project that already depends on
  `lodash@^4`, say — is now fetched once for each question rather than once in
  total, because the answer to one is not the answer to the other. Inside the
  window the range's fetch makes no network request at all, so what this
  actually costs is a single conditional request.
- **The lockfile records peers.** Two new fields per package: `peers`, what
  that node's own peer dependencies resolved to, and `declaredPeers`, the
  ranges and optional flags the package published. Both are omitted when empty,
  so a package with no peers — nearly all of them — writes exactly the bytes it
  wrote before, and `lockfileVersion` stays at `1`: jerky has shipped no 1.0,
  so there is no committed population for a version bump to distinguish.

  A package resolved against peers is keyed `plugin@1.0.0(react@18.2.0)`,
  which is pnpm's spelling, and an edge pointing at one carries the same
  suffix. Such a package is linked with its peers beside its dependencies in
  its own `node_modules`, which is what makes a peer importable at all when
  nothing is hoisted
  ([#103](https://github.com/jerky-build/jerky/issues/103)).

- **Peer dependencies are resolved during an install, and a required peer
  nothing answers is warned about.** One line per complaint, on stderr,
  distinguishing the two things that can go wrong:

  ```
  warning: react-dom@18.2.0 wants peer react@^18.2.0, but the nearest provider has react@17.0.2
  warning: @testing-library/react@14.0.0 wants peer react-dom@^18.0.0, which nothing provides
  ```

  **The install succeeds, and exits 0.** Peer ranges across the live ecosystem
  routinely lag a major release, and a package manager that refused those trees
  would be unable to install most of npm — refusing a tree that works is not a
  stricter kind of correct. A `--strict-peers` that does refuse is policy on
  top of this and will arrive with
  [#41](https://github.com/jerky-build/jerky/issues/41)'s configuration
  surface.

  **It warns on every install, not only the first.** An install whose
  `package.json` files still match the lockfile resolves nothing at all, so the
  ranges each package published are recorded in `declaredPeers` and the warning
  is recomputed from the file. A project does not go quiet with nothing about
  it having changed.

  An optional peer — one the package marked `optional` in
  `peerDependenciesMeta` — says nothing when it is simply absent. A provider
  that is *present but out of range* warns whether the peer is optional or
  not: `optional` says the peer may be missing, not that any version of it
  will do. Either way the peer is left unlinked, so a package gets nothing
  rather than a version it explicitly rejected.

  One complaint is printed once however many copies of the package the peer
  duplication produced, and is named after the package as published —
  `react-dom@18.2.0`, which you can go and find in a `package.json`, rather
  than the store directory it landed in.

  **`--production` reports a peer that only a devDependency answered.** It
  installs `dependencies` only, so a `react` declared under `devDependencies`
  is not there to satisfy a `react-dom` that is — the peer is left unlinked and
  warned about, exactly as any other unanswered one is, rather than linked at a
  package the production tree does not contain
  ([#105](https://github.com/jerky-build/jerky/issues/105)).

### Removed

- Nothing.

### Fixed

- An install can no longer hang forever on a registry that accepts a
  connection and then goes quiet. Every phase of a request now has a deadline:
  ten seconds each to look up the host and to open the socket, thirty each to
  send the request and for the response to start arriving, a minute for a
  metadata body and five for a tarball's — the last two are different numbers
  because a 512 MB tarball on a slow link is a slow download rather than a
  stall, and a packument is never either. Before this, a stalled connection
  was the one failure retrying could not help with — the retry loop runs again
  on an attempt that *returned*, and a hang never does — so jerky would sit at
  zero CPU indefinitely while a `curl` to the same registry answered in a
  tenth of a second.

  A stall is now a transport failure like any other and gets the same three
  attempts, so a registry that goes quiet once costs a pause rather than the
  install. When it does not clear, the error names the URL that stalled and
  which deadline ran out, rather than leaving a hang and a slow registry
  looking alike. Two errors change wording with it: a metadata body that stops
  arriving part-way said "the registry returned a response jerky could not
  understand" and now says the registry stopped responding, and a package
  whose existence jerky could not check — the request behind telling "no such
  package" from "no such version" apart — now reports that rather than
  announcing the package missing
  ([#79](https://github.com/jerky-build/jerky/issues/79)).

- Symlink targets are computed from where the link and its target diverge
  instead of assuming one shape, so links from nested importers and links
  between packages inside the virtual store now resolve. Previously only a
  link in a project's own top-level `node_modules` was correct
  ([#45](https://github.com/jerky-build/jerky/issues/45)).

### Notes

- **`jerky install lodash` still records `"4.17.21"`, not `"^4.17.21"`.** The
  resolver can honour a range now, which is what spec 1 was waiting for, but
  the default is a pin rather than a caret: a caret is standing permission for
  some later install to choose a version nobody asked for, and it is exercised
  on whichever machine happens to re-resolve first. Widening a pin later is an
  edit; discovering that a dependency already drifted is not.
- A range you *ask* for is recorded as you wrote it: `jerky install
  lodash@^4.0.0` puts `"^4.0.0"` in `package.json`, not the version it selected
  today. Every range form npm accepts counts — `~4.17.0`, `4.x`, `>=4 <5`, a
  bare `4` — because the test is whether the request parses as a range at all,
  not which operator it used. The pin is the default for a request that named no version, not an
  override of one that did. A dist-tag still pins — `jerky install lodash@latest`
  records the version `latest` meant, since a tag in a manifest is a moving
  pointer rather than a constraint.
- Lockfile entries no importer can reach are dropped on write. A full
  resolution only ever produced the reachable set, so this keeps a property the
  file already had, which reuse would otherwise end: merging a reused
  importer's packages with a re-resolved one's accumulates entries nothing
  references. A dependency deleted from a `package.json` by hand therefore
  loses its subtree on the next install. This settles the resolver spec's §12,
  which had deferred pruning until an `uninstall` command existed to trigger
  it.

### Errors

- `jerky install <pkg>@*` is refused rather than installed. A range that rules
  nothing out has no good answer: recording `"*"` would put the widest possible
  drift permission in a manifest, and pinning instead would answer a question
  that was not asked. Every spelling is caught — `x`, `X`, `*.*.*`, `>=0.0.0` —
  because the check is on what the range admits rather than on how it was
  typed. Bounded ranges are untouched: `^0.0.0`, `0.x` and `>=1.0.0` each rule
  something out and each still install.
- Running `jerky install <pkg>` from a directory inside the workspace that
  belongs to no member is an error naming the members, rather than a silent
  install into the root. This applies only where the workspace has more than
  one member: ambiguity needs alternatives to exist.
