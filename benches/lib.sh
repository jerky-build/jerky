# Helpers for benches/bench.sh, in a file of their own so benches/lib-test.sh
# can source them without starting a benchmark run. Nothing here shells out to
# a package manager or touches the network.
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
# connection and retried moves a mean and does not move a median, and a
# benchmark against a live registry gets one of those regularly.
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
