# Spell a count with the noun for its number. Sourced, not executed:
#
#   LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
#   source "$LIB_DIR/plural.sh"
#   echo "$(plural "${#FILES[@]}" file files) scanned"
#
# `plural <count> <singular> <plural>` prints `1 file`, `0 files`, `2 files`.
# The shell side of anodizer_core::text::count_noun: a line that puts the
# plural in parentheses reads as a hedge beside every other count, which
# says `1 crate` or `2 crates`.
plural() {
    if [[ "$1" == 1 ]]; then
        printf '%s %s' "$1" "$2"
    else
        printf '%s %s' "$1" "$3"
    fi
}
