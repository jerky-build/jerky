#!/usr/bin/env bash
# Tests for the helpers in benches/lib.sh.
#
# The benchmark itself cannot be tested — it measures a live registry — but the
# arithmetic and the tree-walking under it can be, and those are where a
# silently wrong number would come from: a median that picks the wrong element
# makes every row in the table wrong in a way no reader can see.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

failures=0

check() {
    local what=$1 expected=$2 actual=$3
    if [[ $expected == "$actual" ]]; then
        printf 'ok   %s\n' "$what"
    else
        printf 'FAIL %s: expected %s, got %s\n' "$what" "$expected" "$actual"
        failures=$((failures + 1))
    fi
}

check "median of one" 7 "$(median 7)"
check "median ignores argument order" 5 "$(median 9 1 5)"
# Numeric, not lexicographic: sorted as strings, `100` sorts before `9`.
check "median sorts numerically" 100 "$(median 9 100 1000)"
check "median of an even count takes the lower middle" 3 "$(median 1 3 5 9)"

check "format_ms pads the hundredths" "0.04s" "$(format_ms 40)"
check "format_ms truncates below the hundredth" "0.24s" "$(format_ms 249)"
check "format_ms carries whole seconds" "11.34s" "$(format_ms 11340)"
check "format_ms of nothing" "0.00s" "$(format_ms 0)"

# The clock. This machine must have one of the three sources, and it must
# produce a 13-digit millisecond stamp that moves forward — a timer stuck at a
# constant would report every scenario as instant.
check "a millisecond clock was found" "yes" \
    "$([[ $JERKY_BENCH_TIMER != none ]] && echo yes || echo no)"
first=$(now_ms)
check "now_ms is a 13-digit millisecond stamp" "yes" \
    "$([[ $first =~ ^[0-9]{13}$ ]] && echo yes || echo no)"
sleep 0.2
elapsed=$(($(now_ms) - first))
check "now_ms measured the wait" "yes" \
    "$([[ $elapsed -ge 150 && $elapsed -lt 5000 ]] && echo yes || echo no)"

# `packages_installed` must fail rather than return an empty string: the caller
# prints it in a footer, and an empty one would read as a formatting slip under
# a table of timings that still looked fine.
log=$(mktemp)
trap 'rm -f "$log"' EXIT
printf 'installed 1291 packages across 1 importer\n' >"$log"
check "packages_installed reads the summary line" 1291 "$(packages_installed "$log")"
printf 'installed 1 package across 1 importer\n' >"$log"
check "packages_installed reads the singular" 1 "$(packages_installed "$log")"
printf 'added lodash@4.17.21 to .\n' >"$log"
check "packages_installed fails on a line it does not recognise" "fails" \
    "$(packages_installed "$log" >/dev/null 2>&1 && echo "returned" || echo "fails")"

work=$(mktemp -d)
trap 'rm -f "$log"; rm -rf "$work"' EXIT

# A project with no node_modules at all: zero, not an error.
mkdir -p "$work/empty"
check "count_links on a tree that was never built" 0 "$(count_links "$work/empty")"
check "count_dangling on a tree that was never built" 0 "$(count_dangling "$work/empty")"

# One good link, one dangling, and a real directory that is neither. Nested a
# level down so the walk is proved to descend rather than to list one
# directory.
proj=$work/proj
mkdir -p "$proj/node_modules/.jerky/lodash/node_modules" "$proj/node_modules/real"
ln -s .jerky/lodash/node_modules "$proj/node_modules/lodash"
ln -s ./nowhere "$proj/node_modules/.jerky/lodash/node_modules/broken"

check "count_links counts both links at any depth" 2 "$(count_links "$proj")"
check "count_dangling counts only the broken one" 1 "$(count_dangling "$proj")"
# BSD `wc` pads with spaces, so a bare count would arrive as ` 2`.
check "count_links is not padded" "yes" \
    "$([[ $(count_links "$proj") =~ ^[0-9]+$ ]] && echo yes || echo no)"

# The replay mirror. Its lifecycle and what it serves are testable in exactly
# the way the benchmark is not: a synthetic recording of one package answers
# the same request shapes a real one does, so the path mapping and the
# tarball-URL rewrite can be checked without 470MB of tarballs or a network.
#
# The rewrite is the part worth testing hardest. `dist.tarball` is absolute in
# a real packument, so a mirror that served it verbatim would send every
# tarball request straight back to the live registry — a benchmark that looked
# like it was replaying and was not.

seed=$work/recording
mkdir -p "$seed/packuments" "$seed/tarballs/demo/-" "$seed/tarballs/@scope/pkg/-"
cat >"$seed/recording.json" <<'JSON'
{ "upstream": "https://registry.npmjs.org" }
JSON
cat >"$seed/packuments/demo.json" <<'JSON'
{"name":"demo","dist-tags":{"latest":"1.0.0"},
 "versions":{"1.0.0":{"name":"demo","version":"1.0.0",
   "dist":{"tarball":"https://registry.npmjs.org/demo/-/demo-1.0.0.tgz","integrity":"sha512-x"}}}}
JSON
# A scoped name is a directory separator in a URL and must not become one in a
# filename: jerky asks for `/@scope%2fpkg`, and that is what the file is called.
cat >"$seed/packuments/@scope%2fpkg.json" <<'JSON'
{"name":"@scope/pkg","dist-tags":{"latest":"2.0.0"},
 "versions":{"2.0.0":{"name":"@scope/pkg","version":"2.0.0",
   "dist":{"tarball":"https://registry.npmjs.org/@scope/pkg/-/pkg-2.0.0.tgz","integrity":"sha512-y"}}}}
JSON
printf 'not really a tarball, but these exact bytes\n' >"$seed/tarballs/demo/-/demo-1.0.0.tgz"

check "mirror_seeded says no to a directory with nothing in it" "no" \
    "$(mirror_seeded "$work/empty" && echo yes || echo no)"
check "mirror_seeded says yes to a recording" "yes" \
    "$(mirror_seeded "$seed" && echo yes || echo no)"
check "mirror_summary counts what is there" "2 packuments, 1 tarballs" \
    "$(mirror_summary "$seed" | sed 's/, [0-9.]*[KMGB].*//')"

# The pin gets the same treatment as a packument, and for a reason that is easy
# to miss: a lockfile's `resolved` is an absolute URL too, and the two
# lockfile-bearing scenarios install straight from it without fetching a
# packument at all. A pin copied in verbatim sends every tarball request to the
# live registry while the mirror sits there serving nothing.
cat >"$work/pin.json" <<'JSON'
{"packages":{"a@1.0.0":{"resolved":"https://registry.npmjs.org/a/-/a-1.0.0.tgz"},
             "b@2.0.0":{"resolved":"https://registry.npmjs.org/b/-/b-2.0.0.tgz"}}}
JSON
rewrite_registry "$work/pin.json" "$work/pin-mirror.json" \
    https://registry.npmjs.org http://127.0.0.1:9999
check "rewrite_registry points every resolved URL at the mirror" 2 \
    "$(grep -c 'http://127.0.0.1:9999/' "$work/pin-mirror.json")"
check "rewrite_registry leaves no upstream URL behind" 0 \
    "$(grep -c registry.npmjs.org "$work/pin-mirror.json" || true)"
check "rewrite_registry does not touch the file it copied from" 2 \
    "$(grep -c registry.npmjs.org "$work/pin.json")"

# `--pin` and the npm columns do not go through the mirror, and ask for a copy
# by passing no destination registry.
rewrite_registry "$work/pin.json" "$work/pin-plain.json" https://registry.npmjs.org ""
check "rewrite_registry with nowhere to point is a plain copy" "yes" \
    "$(cmp -s "$work/pin.json" "$work/pin-plain.json" && echo yes || echo no)"

# The mirror starts even when the resolver will not say who 127.0.0.1 is.
#
# `HTTPServer.server_bind` ends with a `socket.getfqdn()` on the address it
# just bound, and on a macOS CI runner that reverse lookup blocked for tens of
# seconds — long enough that `mirror_start` gave up on a process that was alive
# and silent, because the port file is only written once the bind returns. The
# mirror overrides `server_bind` to skip the question; this is what proves it,
# by making the answer arrive far too late to be waited for.
stall=$work/stall
mkdir -p "$stall"
cat >"$stall/sitecustomize.py" <<'PY'
import socket, time
_real = socket.getfqdn
socket.getfqdn = lambda *a, **kw: (time.sleep(120), _real(*a, **kw))[1]
PY
stall_log=$work/stalled-mirror.log
: >"$stall_log"
if PYTHONPATH=$stall mirror_start "$seed" "$stall_log"; then
    check "the mirror starts without a reverse DNS lookup" "started" "started"
    mirror_stop
else
    check "the mirror starts without a reverse DNS lookup" "started" "failed"
fi

mirror_log=$work/mirror.log
: >"$mirror_log"
if mirror_start "$seed" "$mirror_log"; then
    check "mirror_start reports a loopback URL" "yes" \
        "$([[ $MIRROR_URL =~ ^http://127\.0\.0\.1:[0-9]+$ ]] && echo yes || echo no)"

    body=$(curl -fsS "$MIRROR_URL/demo")
    # The whole point: the tarball URL now names the mirror, and the live
    # registry appears nowhere in what jerky is about to read.
    check "a packument's tarball URL is rewritten at the mirror" "yes" \
        "$([[ $body == *"$MIRROR_URL/demo/-/demo-1.0.0.tgz"* ]] && echo yes || echo no)"
    check "no upstream origin survives the rewrite" "yes" \
        "$([[ $body != *registry.npmjs.org* ]] && echo yes || echo no)"

    scoped=$(curl -fsS "$MIRROR_URL/@scope%2fpkg")
    check "a scoped name is served from its escaped filename" "yes" \
        "$([[ $scoped == *'"@scope/pkg"'* ]] && echo yes || echo no)"
    check "a scoped tarball URL is rewritten too" "yes" \
        "$([[ $scoped == *"$MIRROR_URL/@scope/pkg/-/pkg-2.0.0.tgz"* ]] && echo yes || echo no)"

    # A version manifest is answered out of the packument rather than recorded
    # twice. `jerky install <name>` is the caller; a bare install never asks.
    one=$(curl -fsS "$MIRROR_URL/demo/1.0.0")
    check "a version manifest comes out of the packument" "yes" \
        "$([[ $one == *'"version": "1.0.0"'* || $one == *'"version":"1.0.0"'* ]] && echo yes || echo no)"

    # Tarball bytes are served untouched — the rewrite is on metadata only, and
    # a mirror that rewrote inside a tarball would break every integrity check.
    curl -fsS -o "$work/fetched.tgz" "$MIRROR_URL/demo/-/demo-1.0.0.tgz"
    check "a tarball arrives byte for byte" "yes" \
        "$(cmp -s "$work/fetched.tgz" "$seed/tarballs/demo/-/demo-1.0.0.tgz" && echo yes || echo no)"

    # A miss is a 404 rather than a 5xx on purpose: jerky retries 5xx three
    # times with backoff and does not retry a 404, so an incomplete recording
    # fails fast instead of slowly.
    check "a name that was never recorded is a 404" "404" \
        "$(curl -s -o /dev/null -w '%{http_code}' "$MIRROR_URL/never-recorded")"
    check "a path that climbs out of the recording is refused" "400" \
        "$(curl -s -o /dev/null -w '%{http_code}' --path-as-is "$MIRROR_URL/../../etc/passwd.tgz")"

    stopped_pid=$MIRROR_PID
    mirror_stop
    check "mirror_stop leaves no process behind" "gone" \
        "$(kill -0 "$stopped_pid" 2>/dev/null && echo running || echo gone)"
    check "mirror_stop is safe to call twice" "ok" \
        "$(mirror_stop && echo ok || echo failed)"
else
    check "mirror_start starts a mirror" "started" "failed"
fi

if ((failures)); then
    printf '\n%d test(s) failed\n' "$failures"
    exit 1
fi
printf '\nall tests passed\n'
