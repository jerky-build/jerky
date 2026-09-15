# Helpers for benches/bench.sh, in a file of their own so benches/lib-test.sh
# can source them without starting a benchmark run. Nothing here shells out to
# a package manager or touches the network.

# The middle value of a sorted list, or the lower of the two middles for an
# even count. Medians rather than means because one run that lost a TCP
# connection and retried moves a mean and does not move a median, and a
# benchmark against a live registry gets one of those regularly.
median() {
    local sorted
    mapfile -t sorted < <(printf '%s\n' "$@" | sort -n)
    printf '%s\n' "${sorted[$(((${#sorted[@]} - 1) / 2))]}"
}

# Milliseconds as seconds to two decimals: `11340` -> `11.34s`.
#
# Two decimals throughout rather than a significant-figure rule, so a column
# of numbers lines up on the point and `0.04s` and `31.24s` can be read
# against each other without counting digits.
format_ms() {
    local ms=$1
    printf '%d.%02ds\n' $((ms / 1000)) $(((ms % 1000) / 10))
}

# Every symlink under a project's `node_modules`, at any depth. This counts
# the whole tree — an importer's links and the virtual store's alike — because
# what matters is that nothing in the tree dangles, not where it lives.
count_links() {
    local dir=$1
    [[ -d $dir/node_modules ]] || { printf '0\n'; return; }
    find "$dir/node_modules" -type l | wc -l
}

# Symlinks whose target does not resolve.
#
# `-e` follows the link, so it is false exactly when the target is missing.
# A dangling link is a tree that `require` cannot walk, which makes any timing
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
