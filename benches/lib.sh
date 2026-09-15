# Helpers for benches/bench.sh, in a file of their own so benches/lib-test.sh
# can source them without starting a benchmark run. Nothing here shells out to
# a package manager, and the only thing that opens a socket is the replay
# mirror at the bottom, which listens on loopback and talks to the live
# registry only when it is asked to record.
#
# Written for bash 3.2, which is what macOS still ships: no `mapfile`, no
# associative arrays, no `EPOCHREALTIME` assumed. jerky supports macOS, so a
# harness that only runs on the maintainer's Linux box measures nothing there.

# A millisecond clock, resolved once at load rather than per call.
#
# `date +%s%N` is GNU-only: BSD date, which is what macOS has, does not know
# `%N` and emits the literal letter — so the subtraction below would be an
# arithmetic error on a supported platform rather than a wrong number, but only
# after the benchmark had already started. Bash 5's `EPOCHREALTIME` is
# preferred where it exists because it costs no subprocess at all, which
# matters when the thing being timed is a 30ms no-op install.
if [[ -n ${EPOCHREALTIME:-} ]]; then
    JERKY_BENCH_TIMER=epochrealtime
elif [[ $(date +%s%N 2>/dev/null) =~ ^[0-9]+$ ]]; then
    JERKY_BENCH_TIMER=date
elif command -v gdate >/dev/null 2>&1 && [[ $(gdate +%s%N 2>/dev/null) =~ ^[0-9]+$ ]]; then
    JERKY_BENCH_TIMER=gdate
else
    JERKY_BENCH_TIMER=none
fi

# Milliseconds since the epoch. Callers must check `JERKY_BENCH_TIMER` is not
# `none` before the first call; this returns non-zero rather than guessing.
now_ms() {
    local stamp
    case $JERKY_BENCH_TIMER in
        epochrealtime)
            # `1789480000.123456`, but the separator follows the locale, so
            # both are stripped. The first 13 digits are seconds plus
            # milliseconds.
            stamp=${EPOCHREALTIME/[.,]/}
            printf '%s\n' "${stamp:0:13}"
            ;;
        date) printf '%s\n' "$(($(date +%s%N) / 1000000))" ;;
        gdate) printf '%s\n' "$(($(gdate +%s%N) / 1000000))" ;;
        *) return 1 ;;
    esac
}

# The middle value of a sorted list, or the lower of the two middles for an
# even count. Medians rather than means because one run that lost a TCP
# connection and retried moves a mean and does not move a median. The replay
# mirror has removed that particular outlier rather than the reason to be
# robust to one: the machine taking the measurement is still doing other
# things.
median() {
    local sorted=() value
    while IFS= read -r value; do
        sorted+=("$value")
    done < <(printf '%s\n' "$@" | sort -n)
    printf '%s\n' "${sorted[$(((${#sorted[@]} - 1) / 2))]}"
}

# Milliseconds as seconds to two decimals: `11340` -> `11.34s`.
#
# Two decimals throughout rather than a significant-figure rule, so a column of
# numbers lines up on the point and `0.04s` and `31.24s` can be read against
# each other without counting digits.
format_ms() {
    local ms=$1
    printf '%d.%02ds\n' $((ms / 1000)) $(((ms % 1000) / 10))
}

# The package count out of an install's output.
#
# Returns non-zero when the line is not there, which is the point: the caller
# reports this number in a footer, and a silent empty string would print
# `installed  packages` under a table of timings that still looked fine. jerky
# rewording its summary line should stop the benchmark, not decorate it.
packages_installed() {
    local log=$1 count
    count=$(sed -n 's/^installed \([0-9][0-9]*\) package.*/\1/p' "$log")
    [[ -n $count ]] || return 1
    printf '%s\n' "$count"
}

# Every symlink under a project's `node_modules`, at any depth. This counts the
# whole tree — an importer's links and the virtual store's alike — because what
# matters is that nothing in the tree dangles, not where it lives.
#
# `tr -d` because BSD `wc` pads its output with spaces, and a padded number
# interpolated into a printf reads as a broken table.
count_links() {
    local dir=$1
    [[ -d $dir/node_modules ]] || { printf '0\n'; return; }
    find "$dir/node_modules" -type l | wc -l | tr -d '[:space:]'
    printf '\n'
}

# The replay mirror.
#
# A default benchmark run used to make roughly 22,600 anonymous requests at
# registry.npmjs.org — the cold scenario wipes the store before every trial, so
# 2,238 packuments and 2,910 tarballs are fetched again each time. That is a
# rate limit waiting to happen, and it is also why the medians could not be
# reproduced: retry noise from live weather was folded into every number. These
# start and stop `benches/mirror.py`, which replays a recording from disk.

JERKY_BENCH_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
JERKY_BENCH_MIRROR_PY=$JERKY_BENCH_DIR/mirror.py

# Where a recording lives. Gitignored, and overridable, because it is hundreds
# of megabytes of tarballs — far too much to commit, and a thing an operator
# may want on another disk.
mirror_dir() {
    printf '%s\n' "${JERKY_BENCH_MIRROR_DIR:-$JERKY_BENCH_DIR/.mirror}"
}

# Is there a recording to replay? Non-zero when there is not, which is what
# lets bench.sh say so before it starts timing anything.
mirror_seeded() {
    python3 "$JERKY_BENCH_MIRROR_PY" check "$1" >/dev/null 2>&1
}

# What a recording holds, as `N packuments, M tarballs, S`.
mirror_summary() {
    python3 "$JERKY_BENCH_MIRROR_PY" check "$1" 2>/dev/null
}

MIRROR_PID=
MIRROR_URL=
MIRROR_SCRATCH=

# Start a mirror over `dir`, logging to `log`. Any further arguments go to
# `mirror.py serve` — `--record` is the one that matters. Sets `MIRROR_URL` and
# `MIRROR_PID`.
#
# The port is chosen by the kernel rather than fixed, so two benchmarks on one
# machine cannot collide and a recording is not tied to a port number. That is
# also why the URL has to be read back out of the process rather than assumed:
# `mirror.py` writes the bound port to a file through a rename, so a port that
# can be read is a port that is already accepting connections.
mirror_start() {
    local dir=$1 log=$2
    shift 2
    local port_file waited=0

    MIRROR_SCRATCH=$(mktemp -d)
    port_file=$MIRROR_SCRATCH/port

    python3 "$JERKY_BENCH_MIRROR_PY" serve "$dir" --port-file "$port_file" "$@" \
        >>"$log" 2>&1 &
    MIRROR_PID=$!

    # Fifteen seconds: starting is opening a socket, so anything that has not
    # happened by now is a mirror that died, and the log says why.
    while ((waited < 150)); do
        if [[ -s $port_file ]]; then break; fi
        kill -0 "$MIRROR_PID" 2>/dev/null || break
        sleep 0.1
        waited=$((waited + 1))
    done

    if [[ ! -s $port_file ]]; then
        printf 'the replay mirror did not start:\n' >&2
        cat "$log" >&2
        mirror_stop
        return 1
    fi

    MIRROR_URL=http://127.0.0.1:$(cat "$port_file")
}

# Copy a file, pointing every URL at one registry at another one.
#
# For a `jerky-lock.json`. A lockfile records `resolved` as an **absolute** URL
# at the registry that answered, and the two lockfile-bearing scenarios install
# straight from it without ever fetching a packument — so the mirror's
# serve-time rewrite never sees those URLs, and an install from an unrewritten
# pin goes to npm for every single tarball. That is not a hypothetical: it is
# what the first run against the mirror did, under a network namespace with no
# route off the machine, and the error named `registry.npmjs.org`.
#
# A copy rather than an edit in place. The committed pin records what the
# registry actually said, which is the whole point of a pin; only the throwaway
# copy a measuring run installs from names the mirror.
#
# `to` empty, or equal to `from`, is a plain copy — which is what `--pin` and
# the npm columns want, since neither goes through the mirror.
rewrite_registry() {
    local src=$1 dest=$2 from=$3 to=$4
    if [[ -z $to || $from == "$to" ]]; then
        cp "$src" "$dest"
        return
    fi
    # `|` as the delimiter because a URL is full of `/` and has no `|`.
    sed "s|$from|$to|g" "$src" >"$dest"
}

# Stop the mirror, if one is running. Safe to call twice, and safe to call
# when `mirror_start` failed — a trap handler has no way to know which.
mirror_stop() {
    if [[ -n $MIRROR_PID ]]; then
        kill "$MIRROR_PID" 2>/dev/null || true
        wait "$MIRROR_PID" 2>/dev/null || true
    fi
    if [[ -n $MIRROR_SCRATCH ]]; then rm -rf "$MIRROR_SCRATCH"; fi
    MIRROR_PID=
    MIRROR_URL=
    MIRROR_SCRATCH=
}

# Symlinks whose target does not resolve.
#
# `-e` follows the link, so it is false exactly when the target is missing. A
# dangling link is a tree that `require` cannot walk, which makes any timing
# taken against it meaningless — so this is checked on every run rather than
# offered as a flag.
count_dangling() {
    local dir=$1 link count=0
    [[ -d $dir/node_modules ]] || { printf '0\n'; return; }
    while IFS= read -r link; do
        [[ -e $link ]] || count=$((count + 1))
    done < <(find "$dir/node_modules" -type l)
    printf '%s\n' "$count"
}
