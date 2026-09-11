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

# The scanners are written against GNU grep (`-P`) and gawk; macOS ships the
# BSD ones at /usr/bin. Where PATH still resolves a tool to that system copy
# and Homebrew has installed the GNU one (`brew install grep gawk`), its gnubin
# directory goes ahead. A tool that PATH already resolves elsewhere — a test's
# shim, a user's own build — is left in charge.
prefer_gnu_tool() {
    local tool="$1" formula="$2" gnubin
    [[ "$(command -v "$tool")" == "/usr/bin/$tool" ]] || return 0
    for gnubin in "/opt/homebrew/opt/$formula/libexec/gnubin" "/usr/local/opt/$formula/libexec/gnubin"; do
        if [[ -x "$gnubin/$tool" ]]; then
            PATH="$gnubin:$PATH"
            return 0
        fi
    done
}
prefer_gnu_tool grep grep
prefer_gnu_tool awk gawk
export PATH
