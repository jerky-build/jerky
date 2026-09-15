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
# Run `./benches/lib-test.sh` for the helpers this leans on.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO=$PWD
source benches/lib.sh

FIXTURES=(alotta-files alotta-packages)
TRIALS=3
WITH_NPM=0
PIN=0
SELECTED=()

usage() {
    cat <<'USAGE'
usage: ./benches/bench.sh [options]

  --fixture NAME   measure one fixture only (repeatable)
  --trials N       trials per scenario, reported as a median (default 3)
  --npm            measure npm alongside jerky; see the caveat it prints
  --pin            re-resolve each fixture and rewrite its committed
                   jerky-lock.json, then exit without measuring
  -h, --help       this

Fixtures: alotta-files, alotta-packages
USAGE
}

while (($#)); do
    case $1 in
        --fixture) SELECTED+=("$2"); shift 2 ;;
        --trials) TRIALS=$2; shift 2 ;;
        --npm) WITH_NPM=1; shift ;;
        --pin) PIN=1; shift ;;
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

[[ $TRIALS =~ ^[1-9][0-9]*$ ]] || {
    printf -- '--trials wants a positive integer, got: %s\n' "$TRIALS" >&2
    exit 2
}

# A release build, because a debug jerky measures the optimiser rather than the
# install. `JERKY_BIN` is the escape hatch for measuring a binary built
# elsewhere — a previous commit, say, which is the whole point of a benchmark.
JERKY=${JERKY_BIN:-$REPO/target/release/jerky}
if [[ -z ${JERKY_BIN:-} ]]; then
    printf 'building jerky --release\n' >&2
    cargo build --release --quiet
fi
[[ -x $JERKY ]] || { printf 'not executable: %s\n' "$JERKY" >&2; exit 1; }

if [[ $JERKY_BENCH_TIMER == none ]]; then
    printf 'no millisecond clock: this wants bash 5 (for EPOCHREALTIME), GNU date,\n' >&2
    printf 'or gdate from coreutils. On macOS: brew install bash, or brew install coreutils.\n' >&2
    exit 1
fi

if ((WITH_NPM)) && ! command -v npm >/dev/null; then
    printf -- '--npm was asked for and npm is not on PATH\n' >&2
    exit 1
fi

WORK=$(mktemp -d)
# INT and TERM as well as EXIT: a run that is interrupted has a `node_modules`
# of up to a gigabyte in this directory, and a bare EXIT trap does not fire for
# a signal the shell never caught.
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM
export HOME=$WORK/home

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

# Milliseconds spent running the command, which must succeed. `now_ms` rather
# than `time`, so the number arrives as an integer this can do arithmetic on
# instead of a string to be parsed back.
time_ms() {
    local start finish
    start=$(now_ms)
    if ! "$@" >"$WORK/last-run.log" 2>&1; then
        printf '\ncommand failed: %s\n' "$*" >&2
        cat "$WORK/last-run.log" >&2
        exit 1
    fi
    finish=$(now_ms)
    printf '%s\n' $((finish - start))
}

# One trial of one jerky scenario. Returns milliseconds; the untimed setup each
# scenario needs happens first.
#
# `cold` is the only scenario that wipes the store, and it is run first for each
# fixture so the three warm scenarios inherit the store it filled.
jerky_trial() {
    local fixture=$1 scenario=$2
    local pin=$REPO/benches/fixtures/$fixture/jerky-lock.json
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
npm_trial() {
    local fixture=$1 scenario=$2
    export npm_config_cache=$WORK/npm-cache
    local -a npm_args=(install --legacy-peer-deps --no-audit --no-fund)
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

if ((PIN)); then
    pin_fixtures
    exit 0
fi

for fixture in "${FIXTURES[@]}"; do
    [[ -f benches/fixtures/$fixture/jerky-lock.json ]] || {
        printf 'benches/fixtures/%s has no jerky-lock.json. Run --pin first.\n' "$fixture" >&2
        exit 1
    }
done

# Printed under every table that has an npm column, rather than once at the end
# of the run. A reader pastes one fixture's table into an issue, and a
# cross-tool table that arrives without this is misleading about what it
# compares.
npm_caveat() {
    cat <<'CAVEAT'

The two columns do not count the same tree. jerky does not resolve peer
dependencies yet (#34), so it installs fewer packages than npm does from the
same manifest, and npm refuses both fixtures outright without
`--legacy-peer-deps`, which this passes. npm also writes more files than jerky
for the same fixture, because a hoisted tree duplicates what an isolated store
shares. These are different trees, measured for shape rather than as a
scoreboard.
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

for fixture in "${FIXTURES[@]}"; do
    printf '\n## %s (median of %s)\n\n' "$fixture" "$TRIALS"

    if ((WITH_NPM)); then
        printf '| scenario | jerky | npm |\n|---|---|---|\n'
    else
        printf '| scenario | jerky |\n|---|---|\n'
    fi

    packages="" links="" dangling=""
    for scenario in "${SCENARIOS[@]}"; do
        printf '  %s ' "$(label_for "$scenario")" >&2
        times=()
        for ((i = 0; i < TRIALS; i++)); do
            times+=("$(jerky_trial "$fixture" "$scenario")")
            printf '.' >&2
        done
        # Read off the tree the last trial left in the scratch directory,
        # rather than out of `jerky_trial` — a trial runs in a command
        # substitution, so nothing it assigns survives it.
        #
        # Keyed on the scenario rather than on `packages` still being empty.
        # The empty test would re-fire on the next scenario if the summary line
        # ever stopped matching, quietly attributing the no-op row's tree to the
        # cold row; `packages_installed` fails loudly instead.
        if [[ $scenario == cold ]]; then
            packages=$(packages_installed "$WORK/last-run.log")
            links=$(count_links "$WORK/proj")
            dangling=$(count_dangling "$WORK/proj")
        fi
        row="| $(label_for "$scenario") | $(format_ms "$(median "${times[@]}")") |"

        if ((WITH_NPM)); then
            npm_times=()
            for ((i = 0; i < TRIALS; i++)); do
                npm_times+=("$(npm_trial "$fixture" "$scenario")")
                printf '.' >&2
            done
            row="$row $(format_ms "$(median "${npm_times[@]}")") |"
        fi
        printf '\n' >&2
        printf '%s\n' "$row"
    done

    printf '\njerky installed %s packages; %s dangling links out of %s.\n' \
        "$packages" "$dangling" "$links"
    if [[ $dangling != 0 ]]; then
        printf 'A tree with a dangling link in it is one `require` cannot walk, so treat\n'
        printf 'the timings above as measuring something other than a working install.\n'
    fi
    if ((WITH_NPM)); then npm_caveat; fi
done
