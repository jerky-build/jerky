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

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

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

if ((failures)); then
    printf '\n%d test(s) failed\n' "$failures"
    exit 1
fi
printf '\nall tests passed\n'
