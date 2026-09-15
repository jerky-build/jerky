#!/usr/bin/env bash
# Time `jerky install` against the two pnpm/benchmarks fixtures.
#
# Four scenarios per fixture, the same four #70 measured `express@5.2.1` with,
# so express and both fixtures compose into one picture rather than three:
#
#   cold store, no lockfile   a first install on a new machine
#   warm store, no lockfile   the re-resolution an edited package.json forces
#   warm store + lockfile     CI, and a fresh clone
#   no-op, tree already present
#
# Every run is isolated: HOME points at a scratch directory, so the store this
# measures is never the developer's own and "cold" is genuinely cold. The
# vendored fixtures are copied out and never written to.
#
# It is also isolated from the registry. A measuring run replays packuments and
# tarballs from a local recording — see benches/mirror.py — which jerky is
# pointed at with JERKY_REGISTRY_URL, so a default run makes no request at
# registry.npmjs.org at all. `--record` is what takes that recording, and it is
# a separate step for the same reason `--pin` is: re-recording invalidates
# every number taken before it.
#
# Run `./benches/lib-test.sh` for the helpers this leans on.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO=$PWD
source benches/lib.sh

FIXTURES=(alotta-files alotta-packages)
TRIALS=3
WITH_NPM=0
WITH_PNPM=0
PIN=0
RECORD=0
SELECTED=()

# The registry a recording is taken *from*, read before anything points jerky
# at the mirror. Honouring an inherited JERKY_REGISTRY_URL is what lets someone
# record from a private mirror or a proxy rather than from npm directly.
UPSTREAM=${JERKY_REGISTRY_URL:-https://registry.npmjs.org}
MIRROR=$(mirror_dir)

usage() {
    cat <<'USAGE'
usage: ./benches/bench.sh [options]

  --fixture NAME   measure one fixture only (repeatable)
  --trials N       trials per scenario, reported as a median (default 3)
  --npm            measure npm alongside jerky; see the caveat it prints
  --pnpm           measure pnpm alongside jerky, against the same mirror; see
                   the caveat it prints. Wants a recording taken with --pnpm.
  --pin            re-resolve each fixture and rewrite its committed
                   jerky-lock.json, then exit without measuring
  --record         install both fixtures against the live registry, writing
                   down every packument and tarball they ask for, then exit
                   without measuring. Roughly 770MB, and the one thing here
                   that touches the network. Add --pnpm to record what pnpm
                   asks for as well, which jerky never does.
  -h, --help       this

Fixtures: alotta-files, alotta-packages

A measuring run replays the recording rather than fetching anything, so it
makes no request at the live registry. Run --record once before the first
benchmark, and again after --pin.
USAGE
}

while (($#)); do
    case $1 in
        --fixture) SELECTED+=("$2"); shift 2 ;;
        --trials) TRIALS=$2; shift 2 ;;
        --npm) WITH_NPM=1; shift ;;
        --pnpm) WITH_PNPM=1; shift ;;
        --pin) PIN=1; shift ;;
        --record) RECORD=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) printf 'unknown option: %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

if ((${#SELECTED[@]})); then
    for name in "${SELECTED[@]}"; do
        [[ -d benches/fixtures/$name ]] || {
            printf 'no such fixture: %s\n' "$name" >&2
            exit 2
        }
    done
    FIXTURES=("${SELECTED[@]}")
fi

# They are ordered, not simultaneous: a recording taken during a re-pin would
# be a recording of whichever of the two ran first, and which one that was is
# not a thing to leave to argument order.
if ((PIN && RECORD)); then
    printf -- '--pin and --record are separate steps. Pin first, then record.\n' >&2
    exit 2
fi

[[ $TRIALS =~ ^[1-9][0-9]*$ ]] || {
    printf -- '--trials wants a positive integer, got: %s\n' "$TRIALS" >&2
    exit 2
}

# Everything a run needs, asked for before anything is built or timed. The
# release build is last: it is the slowest step here by a wide margin, and a
# missing pnpm or a BSD `date` is not worth a compile to find out about.
if [[ $JERKY_BENCH_TIMER == none ]]; then
    printf 'no millisecond clock: this wants bash 5 (for EPOCHREALTIME), GNU date,\n' >&2
    printf 'or gdate from coreutils. On macOS: brew install bash, or brew install coreutils.\n' >&2
    exit 1
fi

if ((WITH_NPM)) && ! command -v npm >/dev/null; then
    printf -- '--npm was asked for and npm is not on PATH\n' >&2
    exit 1
fi

# pnpm is not vendored here. Checked up here with the rest of them because a
# precheck that runs after the release build makes a typo cost a compile, and
# because a column that discovers halfway through a scenario that it has
# nothing to run prints half a table.
if ((WITH_PNPM)) && ! command -v pnpm >/dev/null; then
    printf -- '--pnpm was asked for and pnpm is not on PATH.\n' >&2
    printf 'Install it (npm install -g pnpm, or corepack enable pnpm) and run this again.\n' >&2
    exit 1
fi

# `--pin` is the one mode that neither serves nor records, so it is the one
# mode that does not want the mirror.
if ((!PIN)) && ! command -v python3 >/dev/null; then
    printf 'the replay mirror is a python3 script and python3 is not on PATH.\n' >&2
    printf 'See benches/README.md; there is nothing to measure against without it.\n' >&2
    exit 1
fi

# A release build, because a debug jerky measures the optimiser rather than the
# install. `JERKY_BIN` is the escape hatch for measuring a binary built
# elsewhere — a previous commit, say, which is the whole point of a benchmark.
JERKY=${JERKY_BIN:-$REPO/target/release/jerky}
if [[ -z ${JERKY_BIN:-} ]]; then
    printf 'building jerky --release\n' >&2
    cargo build --release --quiet
fi
[[ -x $JERKY ]] || { printf 'not executable: %s\n' "$JERKY" >&2; exit 1; }

WORK=$(mktemp -d)
# INT and TERM as well as EXIT: a run that is interrupted has a `node_modules`
# of up to a gigabyte in this directory, and a bare EXIT trap does not fire for
# a signal the shell never caught.
cleanup() { mirror_stop; rm -rf "$WORK"; }
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM
export HOME=$WORK/home

# Kept out of the timing path but named early, because `time_ms` reads it: an
# install that fails against an incomplete recording says so in here, and the
# jerky log alone would only show a 404 without saying what missed.
MIRROR_LOG=$WORK/mirror.log
: >"$MIRROR_LOG"

# The empty file `run_pnpm` points pnpm's user and global config at. See there
# for why a project `.npmrc` is not enough on its own.
: >"$WORK/empty-npmrc"

# Lay out a project to install into: the fixture's manifest, and a lockfile
# only when the scenario is one that has one.
#
# `dir` because jerky's project and npm's are kept apart. `npm install` writes a
# package-lock.json into whatever directory it runs in, and a stray one in
# jerky's project would silently change what the no-lockfile rows measure.
# `lockfile` is a path to copy in, or empty for the rows that start from the
# manifest alone.
prepare_project() {
    local dir=$1 fixture=$2 lockfile=${3:-}
    rm -rf "$dir"
    mkdir -p "$dir"
    cp "$REPO/benches/fixtures/$fixture/package.json" "$dir/package.json"
    if [[ -n $lockfile ]]; then
        cp "$lockfile" "$dir/$(basename "$lockfile")"
    fi
}

# pnpm's project: jerky's, plus the `.npmrc` that tells pnpm where everything
# is.
#
# The registry lines come from `pnpm_registry_npmrc`, which is where the
# scope-specific half of that question is dealt with. A project `.npmrc` is the
# highest-precedence file pnpm reads, so these beat anything configured on the
# machine — but only key for key, which is the trap that function exists to
# close.
#
# Not a command substitution: `pnpm_registry_npmrc` refuses to return a
# registry list it could not make safe, and an `exit` inside `$(...)` would
# leave the subshell and let the run carry on measuring.
#
# The three directory settings move what pnpm would otherwise keep under `$HOME`
# out into the scratch tree. That is not tidiness: the cold *jerky* trial
# deletes `$HOME` outright, so a pnpm store under it would be wiped by the other
# column's setup and every pnpm row would measure a cold install. Out here they
# are wiped exactly when pnpm's own cold row wipes them.
#
# `update-notifier` off because it is a request at registry.npmjs.org for
# pnpm's own latest version, which is precisely the live traffic this harness
# exists to remove. Whether the lockfile is frozen is deliberately *not* here:
# it differs per scenario, and setting it project-wide is what made the
# lockfile rows measure a re-check jerky's lockfile row does not do.
prepare_pnpm_project() {
    local dir=$1 fixture=$2 lockfile=${3:-}
    prepare_project "$dir" "$fixture" "$lockfile"
    pnpm_registry_npmrc >"$dir/.npmrc"
    cat >>"$dir/.npmrc" <<NPMRC
store-dir=$WORK/pnpm-store
cache-dir=$WORK/pnpm-cache
state-dir=$WORK/pnpm-state
update-notifier=false
NPMRC
}

# Milliseconds spent running the command, which must succeed. `now_ms` rather
# than `time`, so the number arrives as an integer this can do arithmetic on
# instead of a string to be parsed back.
time_ms() {
    local start finish
    start=$(now_ms)
    if ! "$@" >"$WORK/last-run.log" 2>&1; then
        printf '\ncommand failed: %s\n' "$*" >&2
        cat "$WORK/last-run.log" >&2
        if [[ -s $MIRROR_LOG ]]; then
            printf '\nlast lines from the replay mirror:\n' >&2
            tail -n 20 "$MIRROR_LOG" >&2
        fi
        exit 1
    fi
    finish=$(now_ms)
    printf '%s\n' $((finish - start))
}

# The pin a jerky scenario installs from, with its `resolved` URLs pointed
# wherever jerky is pointed.
#
# The committed lockfile names `registry.npmjs.org`, because that is the
# registry that answered when it was taken. The lockfile rows install straight
# from it and never fetch a packument, so nothing the mirror does when *serving*
# one can redirect those URLs — see `rewrite_registry`. Cached under `$WORK`
# rather than rewritten per trial, since it is the same file every time and
# `alotta-packages` is 1.1MB of it.
#
# npm's lockfile deliberately does not go through this: npm is not pointed at
# the mirror, so a rewritten one would send it for tarballs the recording was
# never asked to hold.
pin_for() {
    local fixture=$1
    local dest=$WORK/pins/$fixture/jerky-lock.json
    if [[ ! -f $dest ]]; then
        mkdir -p "$WORK/pins/$fixture"
        rewrite_registry "$REPO/benches/fixtures/$fixture/jerky-lock.json" \
            "$dest" "$UPSTREAM" "${JERKY_REGISTRY_URL:-}"
    fi
    printf '%s\n' "$dest"
}

# One trial of one jerky scenario. Returns milliseconds; the untimed setup each
# scenario needs happens first.
#
# `cold` is the only scenario that wipes the store, and it is run first for each
# fixture so the three warm scenarios inherit the store it filled.
jerky_trial() {
    local fixture=$1 scenario=$2
    local pin
    pin=$(pin_for "$fixture")
    case $scenario in
        cold)
            rm -rf "$HOME"
            mkdir -p "$HOME"
            prepare_project "$WORK/proj" "$fixture"
            ;;
        warm) prepare_project "$WORK/proj" "$fixture" ;;
        lockfile) prepare_project "$WORK/proj" "$fixture" "$pin" ;;
        noop)
            prepare_project "$WORK/proj" "$fixture" "$pin"
            (cd "$WORK/proj" && time_ms "$JERKY" install >/dev/null)
            ;;
    esac

    (cd "$WORK/proj" && time_ms "$JERKY" install)
}

# npm's equivalents of the same four scenarios.
#
# npm has no separate store and lockfile: `npm install` always writes a
# package-lock.json, so "no lockfile" means deleting the one the previous run
# wrote, and the cache is what "warm" refers to. `--legacy-peer-deps` is not a
# thumb on the scale — npm refuses both fixtures outright without it.
#
# `--ignore-scripts` for the reason pnpm's column passes it: jerky runs no
# lifecycle scripts at all, so a table whose columns disagreed about whether to
# run them would be comparing an install against an install plus a node-gyp
# build. One convention across every column, stated in both caveats.
npm_trial() {
    local fixture=$1 scenario=$2
    export npm_config_cache=$WORK/npm-cache
    local -a npm_args=(install --legacy-peer-deps --no-audit --no-fund --ignore-scripts)
    local pin=$WORK/npm-pin-$fixture/package-lock.json

    # The lockfile rows produce their own lockfile in untimed setup rather than
    # inheriting whatever the previous scenario left behind. Depending on the
    # order would make "warm store + lockfile" quietly measure a no-lockfile
    # install the first time it ran, which is a wrong number and not an error.
    if [[ $scenario == lockfile || $scenario == noop ]] && [[ ! -f $pin ]]; then
        prepare_project "$WORK/npm-proj" "$fixture"
        (cd "$WORK/npm-proj" && time_ms npm "${npm_args[@]}" >/dev/null)
        mkdir -p "$(dirname "$pin")"
        cp "$WORK/npm-proj/package-lock.json" "$pin"
    fi

    case $scenario in
        cold)
            rm -rf "$WORK/npm-cache"
            prepare_project "$WORK/npm-proj" "$fixture"
            ;;
        warm) prepare_project "$WORK/npm-proj" "$fixture" ;;
        lockfile) prepare_project "$WORK/npm-proj" "$fixture" "$pin" ;;
        noop)
            prepare_project "$WORK/npm-proj" "$fixture" "$pin"
            (cd "$WORK/npm-proj" && time_ms npm "${npm_args[@]}" >/dev/null)
            ;;
    esac

    (cd "$WORK/npm-proj" && time_ms npm "${npm_args[@]}")
}

# The arguments every pnpm invocation here shares. A file-scope array rather
# than a `local -a` inside `pnpm_trial`, which is what npm's equivalent is,
# because `record_mirror` drives a pnpm install too and the two have to agree:
# a recording taken with one set of arguments and replayed against another is
# a recording of the wrong install. The scenario-dependent part — whether the
# lockfile is frozen — is added at the call site.
#
# `--ignore-scripts` because jerky runs no lifecycle scripts at all; there is no
# `Command` anywhere in `src/`. npm's column passes it too, so the whole table
# is one convention. It also keeps the promise this harness makes about the
# network: a `postinstall` that downloads a prebuilt binary reaches past the
# mirror for it.
PNPM_ARGS=(install --ignore-scripts)

# Every pnpm invocation goes through this, for the two environment variables.
#
# `registry=` in a project `.npmrc` does **not** override a `@scope:registry=`
# line in a user or global one — npm's config is per-scope, and the more
# specific key wins wherever it was written. Both fixtures are full of scoped
# names (`alotta-packages` is `@angular/cli`, `@nestjs/cli`, `@vue/cli-service`
# and eleven more), so a developer with a scoped registry configured would have
# a measured install go to it. That failure is the dangerous kind: the mirror
# 404s loudly on a miss, but a request that never reaches the mirror at all is
# silent, live, and in the middle of a number this harness promises is offline.
#
# Emptying `userconfig` and `globalconfig` is what removes those layers. It is
# not enough on its own — pnpm ships `@jsr:registry` as a built-in default,
# which no file can unset — so `pnpm_registry_npmrc` below re-points whatever
# survives. The two together are what makes the guarantee whole.
run_pnpm() {
    npm_config_userconfig=$WORK/empty-npmrc \
        npm_config_globalconfig=$WORK/empty-npmrc \
        pnpm "$@"
}

# The registry lines for a pnpm project: the default, plus an override for
# every scope pnpm still believes has a registry of its own.
#
# Asked of pnpm rather than derived from the fixture's manifest, because a
# scoped registry can be configured for a scope no manifest mentions — a
# transitive `@babel/...` is reached through no line of `package.json` — and
# because the built-in `@jsr` is in none of them. Whatever pnpm says it would
# use is what gets pointed at the mirror.
#
# Computed once per run and cached: it is the same answer every time, and
# `prepare_pnpm_project` runs twice per trial.
pnpm_registry_npmrc() {
    local cache=$WORK/pnpm-registry.npmrc probe=$WORK/pnpm-probe scope
    if [[ -f $cache ]]; then
        cat "$cache"
        return
    fi

    rm -rf "$probe"
    mkdir -p "$probe"
    printf 'registry=%s\n' "$MIRROR_URL" >"$probe/.npmrc"
    printf 'registry=%s\n' "$MIRROR_URL" >"$cache"
    (cd "$probe" && run_pnpm config list) |
        sed -n 's/^\(@[^:]*\):registry=.*/\1/p' |
        while IFS= read -r scope; do
            printf '%s:registry=%s\n' "$scope" "$MIRROR_URL" >>"$cache"
        done

    # Loud rather than silent, and checked rather than assumed: this is the one
    # setting whose failure mode is a live request nothing else here would
    # notice.
    cp "$cache" "$probe/.npmrc"
    if (cd "$probe" && run_pnpm config list) |
        grep ':registry=' |
        grep -qv ":registry=$MIRROR_URL/*\$"; then
        printf 'a scope-specific registry survives that does not name the mirror:\n' >&2
        (cd "$probe" && run_pnpm config list) | grep ':registry=' >&2
        printf 'pnpm would fetch those scopes live, so this is refusing to measure.\n' >&2
        exit 1
    fi
    cat "$cache"
}

# pnpm's equivalents of the same four scenarios.
#
# pnpm is the one other column here pointed at the replay mirror, so its
# numbers and jerky's are taken against the same bytes over the same loopback
# socket. The mapping is nearest-equivalent rather than identical, because the
# two tools do not divide their state the same way:
#
#   cold      pnpm's content-addressable store *and* its metadata cache, both
#             wiped. Those are the two things jerky's cold row wipes when it
#             deletes `$HOME`, so "warm" means the same pair on both sides.
#   warm      both inherited from the cold row, no lockfile in the project.
#   lockfile  installed frozen from a `pnpm-lock.yaml`. pnpm's own, generated in
#             untimed setup: there is no committed pnpm pin the way there is a
#             jerky-lock.json, and there should not be one — a pin generated by
#             the tool being measured is what pnpm's CI row actually installs.
#   noop      the same, run once untimed first so the timed run finds the tree
#             already there.
#
# Like npm, pnpm writes a lockfile on every install, so "no lockfile" is a
# property of the directory the run starts in rather than a flag.
#
# **Frozen is per scenario, and getting that wrong is not symmetric.** A
# non-frozen install re-checks the lockfile against `package.json` before it
# does anything, which is work jerky's lockfile row does not do — so leaving it
# off across the board put a re-resolution pnpm would never perform in CI into
# the two rows jerky already wins, and made the margin look bigger than it is.
# Frozen is also what pnpm turns on by itself when `CI` is set, which is the
# scenario those two rows are named after. Off for the two no-lockfile rows,
# where there is no lockfile to freeze and frozen is an error.
pnpm_trial() {
    local fixture=$1 scenario=$2
    local pin=$WORK/pnpm-pin-$fixture/pnpm-lock.yaml
    local -a args=("${PNPM_ARGS[@]}")
    case $scenario in
        cold | warm) args+=(--no-frozen-lockfile) ;;
        lockfile | noop) args+=(--frozen-lockfile) ;;
    esac

    # Generated in untimed setup rather than inherited from whichever scenario
    # ran last, for the reason npm's is: depending on the order would make
    # "warm store + lockfile" quietly measure a no-lockfile install the first
    # time it ran, which is a wrong number rather than an error. Necessarily
    # not frozen — this is the install that writes the lockfile the frozen ones
    # then install from.
    if [[ $scenario == lockfile || $scenario == noop ]] && [[ ! -f $pin ]]; then
        prepare_pnpm_project "$WORK/pnpm-proj" "$fixture"
        (cd "$WORK/pnpm-proj" && time_ms run_pnpm "${PNPM_ARGS[@]}" --no-frozen-lockfile >/dev/null)
        mkdir -p "$(dirname "$pin")"
        cp "$WORK/pnpm-proj/pnpm-lock.yaml" "$pin"
    fi

    case $scenario in
        cold)
            rm -rf "$WORK/pnpm-store" "$WORK/pnpm-cache" "$WORK/pnpm-state"
            prepare_pnpm_project "$WORK/pnpm-proj" "$fixture"
            ;;
        warm) prepare_pnpm_project "$WORK/pnpm-proj" "$fixture" ;;
        lockfile) prepare_pnpm_project "$WORK/pnpm-proj" "$fixture" "$pin" ;;
        noop)
            prepare_pnpm_project "$WORK/pnpm-proj" "$fixture" "$pin"
            (cd "$WORK/pnpm-proj" && time_ms run_pnpm "${args[@]}" >/dev/null)
            ;;
    esac

    (cd "$WORK/pnpm-proj" && time_ms run_pnpm "${args[@]}")
}

# One trial of one scenario, for whichever tool the column belongs to. The
# dispatch is here so the table loop below iterates over column names rather
# than branching on a flag per column, which is what stops the header and the
# rows from disagreeing about how many cells there are.
#
# The three `_trial` functions above share a shape — a four-arm `case` and a
# trailing timed install — and are deliberately **not** folded into one
# parameterised function. What differs between them is not a value or two:
#
#   - jerky wipes `$HOME`; npm wipes one cache directory; pnpm wipes three.
#   - jerky installs from a committed pin, rewritten to name the mirror. npm
#     and pnpm each generate their own, because neither has a committed one.
#   - each takes a different argument array, and pnpm's varies by scenario.
#   - only pnpm needs an `.npmrc` written before every install.
#
# A single function taking those would take four arrays and two paths. bash 3.2
# has no associative arrays and cannot return an array, so they would arrive as
# positional parameters or through `eval`-style indirect expansion — and the
# failure mode of getting one wrong is a scenario that silently measures
# something else, which is the exact class of bug this harness is built to
# avoid. The shape was worth extracting where it could be checked by a test
# (`table_header`, `table_row`, and this dispatch); the bodies were not.
#
# The cost is real and is the reason to write this down rather than leave it
# implied: a fifth scenario means editing `SCENARIOS`, `label_for`, and the
# `case` in each of the three. If that happens twice, revisit this.
trial() {
    case $1 in
        jerky) jerky_trial "$2" "$3" ;;
        npm) npm_trial "$2" "$3" ;;
        pnpm) pnpm_trial "$2" "$3" ;;
    esac
}

# Re-resolve each fixture and commit the result as its pin.
#
# alotta-files is carets all the way down and alotta-packages pins only its
# fourteen direct dependencies, so without this the graph drifts under us and a
# number taken today is not comparable to one taken next month. jerky pins
# exact by default, which makes a lockfile per fixture both the pin and a thing
# worth having. Re-pinning is deliberately a separate, reviewable commit.
pin_fixtures() {
    for fixture in "${FIXTURES[@]}"; do
        printf 'pinning %s\n' "$fixture" >&2
        rm -rf "$HOME"
        mkdir -p "$HOME"
        prepare_project "$WORK/proj" "$fixture"
        (cd "$WORK/proj" && time_ms "$JERKY" install >/dev/null)
        cp "$WORK/proj/jerky-lock.json" "$REPO/benches/fixtures/$fixture/jerky-lock.json"
        printf '  wrote benches/fixtures/%s/jerky-lock.json (%s packages)\n' \
            "$fixture" "$(packages_installed "$WORK/last-run.log")" >&2
    done
}

# Take the recording the measuring runs replay.
#
# By observation rather than from a list: the mirror is started as a caching
# proxy and a real install is driven through it, so whatever jerky asks for is
# what gets written down. A list derived from the pins would look equivalent
# and is not — the two no-lockfile scenarios re-resolve from the manifest, and
# a range that has picked up a newer version since the pin was taken wants a
# tarball no pin names.
#
# Two installs per fixture for the same reason: one from the manifest alone,
# which is what the cold and warm rows do, and one from the pin, which is what
# the lockfile and no-op rows do. The store is wiped before each so every
# tarball is genuinely requested rather than found locally — that costs local
# traffic against the proxy's own cache, not upstream bandwidth.
record_mirror() {
    local fixture pin
    mkdir -p "$MIRROR"
    printf 'recording from %s into %s\n' "$UPSTREAM" "$MIRROR" >&2
    printf 'This is the one thing here that touches the network, and it is\n' >&2
    printf 'roughly 770MB for both fixtures. It resumes, so an interrupted\n' >&2
    printf 'recording can be finished by running this again.\n\n' >&2
    if ((WITH_PNPM)); then
        printf 'Recording what pnpm asks for as well as what jerky does, which is\n' >&2
        printf 'the peer dependencies jerky never resolves — another 12%% or so on\n' >&2
        printf 'top of that, measured on alotta-files.\n\n' >&2
    fi

    mirror_start "$MIRROR" "$MIRROR_LOG" --record --upstream "$UPSTREAM"
    export JERKY_REGISTRY_URL=$MIRROR_URL

    for fixture in "${FIXTURES[@]}"; do
        printf '  %s: resolving from the manifest\n' "$fixture" >&2
        rm -rf "$HOME"
        mkdir -p "$HOME"
        prepare_project "$WORK/proj" "$fixture"
        (cd "$WORK/proj" && time_ms "$JERKY" install >/dev/null)

        if [[ -f $REPO/benches/fixtures/$fixture/jerky-lock.json ]]; then
            pin=$(pin_for "$fixture")
            printf '  %s: installing from the pin\n' "$fixture" >&2
            rm -rf "$HOME"
            mkdir -p "$HOME"
            prepare_project "$WORK/proj" "$fixture" "$pin"
            (cd "$WORK/proj" && time_ms "$JERKY" install >/dev/null)
        fi

        # The same observation trick, for the other tool that will replay this.
        # pnpm asks for packages jerky never does — it installs peer
        # dependencies and jerky does not yet (#34) — so a recording taken for
        # jerky alone 404s the first peer pnpm reaches, in the middle of a
        # timed run. One install from the manifest covers all four pnpm rows:
        # the lockfile rows install from a `pnpm-lock.yaml` pnpm generates from
        # this same manifest against this same frozen recording, so they can
        # want nothing this did not ask for.
        #
        # The store and cache are wiped first so every tarball is genuinely
        # requested rather than found locally, and the marker is written at the
        # end of the fixture, after the install that earns it.
        if ((WITH_PNPM)); then
            printf '  %s: resolving with pnpm\n' "$fixture" >&2
            rm -rf "$WORK/pnpm-store" "$WORK/pnpm-cache" "$WORK/pnpm-state"
            prepare_pnpm_project "$WORK/pnpm-proj" "$fixture"
            (cd "$WORK/pnpm-proj" && time_ms run_pnpm "${PNPM_ARGS[@]}" --no-frozen-lockfile >/dev/null)
            mirror_mark_covered "$MIRROR" pnpm "$fixture"
        fi
    done

    mirror_stop
    printf '\nrecorded %s into %s\n' "$(mirror_summary "$MIRROR")" "$MIRROR" >&2
    printf 'Nothing here is committed — it is gitignored, and re-recording is\n' >&2
    printf 'what invalidates numbers rather than what a diff should carry.\n' >&2
}

if ((PIN)); then
    # Deliberately against the live registry: re-pinning is the act of asking
    # what the ranges resolve to *now*, which a recording by definition cannot
    # answer. Re-record afterwards, or the mirror is a recording of the old pin.
    printf 'pinning resolves against %s; run --record afterwards\n\n' "$UPSTREAM" >&2
    pin_fixtures
    exit 0
fi

if ((RECORD)); then
    record_mirror
    exit 0
fi

for fixture in "${FIXTURES[@]}"; do
    [[ -f benches/fixtures/$fixture/jerky-lock.json ]] || {
        printf 'benches/fixtures/%s has no jerky-lock.json. Run --pin first.\n' "$fixture" >&2
        exit 1
    }
done

if ! mirror_seeded "$MIRROR"; then
    cat >&2 <<MSG
No recording at $MIRROR, so there is nothing to measure against.

A measuring run replays the registry from disk. Fetching it live instead would
be ~22,600 anonymous requests per default run — a rate limit, and the reason
the medians could not be reproduced, since a retry after a lost connection is
weather rather than a cost of the install. Seed the mirror once with

    ./benches/bench.sh --record

which installs both fixtures live and writes down everything they ask for,
roughly 770MB. It is gitignored; set JERKY_BENCH_MIRROR_DIR to keep it
somewhere else.
MSG
    exit 1
fi

# Asked before anything is timed, rather than discovered as a 404 four minutes
# into a scenario. A recording holds what the tool that was driven through the
# proxy asked for, and pnpm asks for more than jerky does.
if ((WITH_PNPM)); then
    for fixture in "${FIXTURES[@]}"; do
        mirror_covers "$MIRROR" pnpm "$fixture" || {
            cat >&2 <<MSG
The recording at $MIRROR does not cover pnpm for $fixture.

pnpm and jerky do not ask for the same packages: pnpm installs peer
dependencies and jerky does not yet (#34), so a recording taken for jerky alone
404s at the first peer pnpm reaches — in the middle of a timed run. Extend it
with

    ./benches/bench.sh --record --pnpm --fixture $fixture

which drives a pnpm install through the recording proxy and writes down
whatever jerky never asked for. Anything already recorded is a hit, so this
costs only the difference.
MSG
            exit 1
        }
    done
fi

mirror_start "$MIRROR" "$MIRROR_LOG"
# The whole point of the issue, in one line: jerky is pointed at the mirror,
# and `main` is the only place that reads this.
export JERKY_REGISTRY_URL=$MIRROR_URL
printf 'replaying %s from %s\n' "$(mirror_summary "$MIRROR")" "$MIRROR" >&2

# Printed under every table that has an npm column, rather than once at the end
# of the run. A reader pastes one fixture's table into an issue, and a
# cross-tool table that arrives without this is misleading about what it
# compares.
npm_caveat() {
    cat <<'CAVEAT'

jerky and npm do not count the same tree. jerky does not resolve peer
dependencies yet (#34), so it installs fewer packages than npm does from the
same manifest, and npm refuses both fixtures outright without
`--legacy-peer-deps`, which this passes. npm also writes more files than jerky
for the same fixture, because a hoisted tree duplicates what an isolated store
shares. These are different trees, measured for shape rather than as a
scoreboard.

Every column here runs with lifecycle scripts off. jerky runs none at all, so
a column that ran them would be timing an install plus a node-gyp build
against one that was not.

The two columns also do not talk to the same registry: jerky replays a local
recording and npm fetches live, so npm's numbers carry the network and jerky's
do not. `--npm` is the only mode here that reaches registry.npmjs.org while
measuring, and its rows should be read as a shape comparison rather than a
like-for-like time.
CAVEAT
}

# The same, for pnpm, and printed under every table that has that column for
# the same reason: a reader pastes one fixture's table into an issue.
pnpm_caveat() {
    cat <<'CAVEAT'

The pnpm column and the jerky column replay the same local recording over
loopback. Neither carries the network, so those two times can be read against
each other as times — which is what makes pnpm a comparison here rather than
the shape check an npm column is. (If this table has an npm column too, that
one still fetches live; its own caveat says so.)

jerky and pnpm do not count the same tree. jerky does not resolve peer
dependencies yet (#34), so pnpm installs packages jerky never asks for; that is
also why the recording has to be taken with `--pnpm` before this run can happen
at all. The rows are two package managers each doing their own job on one
manifest, not a scoreboard.

pnpm's scenarios are the nearest equivalents rather than identical ones. Its
"warm" is its content-addressable store plus its metadata cache, which is the
pair jerky's warm rows mean; "no lockfile" means starting from a directory that
has no `pnpm-lock.yaml`, since pnpm writes one on every install; and the
lockfile rows install from a lockfile pnpm generated itself, frozen, because
there is no committed pnpm pin the way there is a jerky-lock.json. Frozen is
what pnpm does in CI, and it is what stops those rows timing a re-check of the
lockfile against `package.json` that jerky's lockfile row does not perform.
CAVEAT
}

SCENARIOS=(cold warm lockfile noop)
label_for() {
    case $1 in
        cold) printf 'cold store, no lockfile\n' ;;
        warm) printf 'warm store, no lockfile\n' ;;
        lockfile) printf 'warm store + lockfile\n' ;;
        noop) printf 'no-op, tree already present\n' ;;
    esac
}

# jerky first, always: it is the column every table has, its cold row is what
# fills the store the three warm rows inherit, and the footer's tree figures are
# read off the directory its last trial left behind.
#
# `TOOLS` rather than the obvious `COLUMNS`, which is a variable bash itself
# owns: it holds the terminal width, and a shell with `checkwinsize` on
# rewrites it after every command. A table whose column list is silently
# replaced by `80` is a thing to not find out about later.
TOOLS=(jerky)
if ((WITH_NPM)); then TOOLS+=(npm); fi
if ((WITH_PNPM)); then TOOLS+=(pnpm); fi

for fixture in "${FIXTURES[@]}"; do
    printf '\n## %s (median of %s)\n\n' "$fixture" "$TRIALS"
    table_header scenario "${TOOLS[@]}"

    packages="" links="" dangling=""
    for scenario in "${SCENARIOS[@]}"; do
        printf '  %s ' "$(label_for "$scenario")" >&2
        medians=()
        for tool in "${TOOLS[@]}"; do
            times=()
            for ((i = 0; i < TRIALS; i++)); do
                times+=("$(trial "$tool" "$fixture" "$scenario")")
                printf '.' >&2
            done
            medians+=("$(median "${times[@]}")")

            # Read off the tree the last trial left in the scratch directory,
            # rather than out of `jerky_trial` — a trial runs in a command
            # substitution, so nothing it assigns survives it. jerky's column
            # goes first, so this happens before any other tool has written
            # over `last-run.log`.
            #
            # Keyed on the scenario rather than on `packages` still being
            # empty. The empty test would re-fire on the next scenario if the
            # summary line ever stopped matching, quietly attributing the no-op
            # row's tree to the cold row; `packages_installed` fails loudly
            # instead.
            if [[ $tool == jerky && $scenario == cold ]]; then
                packages=$(packages_installed "$WORK/last-run.log")
                links=$(count_links "$WORK/proj")
                dangling=$(count_dangling "$WORK/proj")
            fi
        done
        printf '\n' >&2
        table_row "$(label_for "$scenario")" "${medians[@]}"
    done

    printf '\njerky installed %s packages; %s dangling links out of %s.\n' \
        "$packages" "$dangling" "$links"
    if [[ $dangling != 0 ]]; then
        printf 'A tree with a dangling link in it is one `require` cannot walk, so treat\n'
        printf 'the timings above as measuring something other than a working install.\n'
    fi
    if ((WITH_NPM)); then npm_caveat; fi
    if ((WITH_PNPM)); then pnpm_caveat; fi
done
