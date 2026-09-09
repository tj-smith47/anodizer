# The bash floor every audit scanner shares. Sourced, not executed:
#
#   LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
#   source "$LIB_DIR/require-bash.sh"
#
# bash >= 4.4: `mapfile` arrived in 4.0, and 4.4 is where `set -u` stopped
# treating an empty array's `"${arr[@]}"` as an unset expansion. Stating the
# floor is what lets every array in every scanner be expanded plainly instead
# of half of them carrying a `${arr[@]+…}` guard the other half forgot — and
# stating it ONCE is what stops that floor from holding in only some of them.
((BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4))) || {
    echo "$(basename "${BASH_SOURCE[1]:-$0}" .sh): needs bash >= 4.4, found $BASH_VERSION." >&2
    exit 2
}
