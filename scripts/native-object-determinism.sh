#!/usr/bin/env bash
# Fast per-PR guard that the C-heavy deps still compile to byte-identical objects
# under clang-cl. It catches clang-cl itself regressing to non-deterministic codegen
# (e.g. an LLVM bump) or a newly-added C dependency that is non-deterministic even
# under clang-cl — before the release-time determinism check would. (A clang-cl pin
# dropped from anodizer's own shipping code is caught by the Rust unit tests, not here:
# this script sets the pins in its own env.) Real deps are built via `-p` from the
# workspace lockfile (never a synthetic Cargo.toml) so there is zero drift between
# what this guards and what actually ships.
#
# NASM-assembled objects (aws-lc-sys perlasm, blake3 asm) differ only in the
# COFF TimeDateStamp (bytes 4-7) between rebuilds — link /Brepro normalizes
# that in the final PE, so it is benign and must be excluded from the compare
# rather than papered over by excluding whole crates. `.lib` archives are
# skipped entirely: ar member timestamps are archive metadata, not codegen.
set -euo pipefail

DEPS=(aws-lc-sys ring zstd-sys lzma-sys blake3)
TARGET_TRIPLE=x86_64-pc-windows-msvc

require_on_path() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "native-object-determinism: '$1' not found on PATH — this guard needs it (runner-image regression if this was expected to be preinstalled)." >&2
        exit 1
    }
}

require_on_path clang-cl
require_on_path nasm

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT
TARGET_DIR="${WORKDIR}/cargo-target"
mkdir -p "$TARGET_DIR"

PKG_ARGS=()
for dep in "${DEPS[@]}"; do
    PKG_ARGS+=(-p "$dep")
done

# Bash cannot `export` a name containing a hyphen (the underscore triple form
# below is exported for symmetry, but rustc/cc-rs also probe the hyphenated
# form cargo itself uses) — `env NAME=value cmd` sets it in the child's
# environment without going through the shell's identifier restriction.
CARGO_ENV=(
    "CC_x86_64-pc-windows-msvc=clang-cl"
    "CC_x86_64_pc_windows_msvc=clang-cl"
    "CXX_x86_64-pc-windows-msvc=clang-cl"
    "CXX_x86_64_pc_windows_msvc=clang-cl"
    "CARGO_INCREMENTAL=0"
    "CARGO_TARGET_DIR=${TARGET_DIR}"
)

# Normalizes a .obj/.o's COFF TimeDateStamp (offset 4, 4 bytes) out of the hash
# input entirely — hash everything except those 4 bytes, rather than zeroing
# them in place, so no temp copy is needed per object.
hash_object() {
    { head -c 4 "$1"; tail -c +9 "$1"; } | sha256sum | awk '{print $1}'
}

snapshot() {
    local label="$1"
    local snap="${WORKDIR}/${label}.snapshot"
    : > "$snap"
    while IFS= read -r -d '' f; do
        rel="${f#"$TARGET_DIR"/}"
        printf '%s  %s\n' "$(hash_object "$f")" "$rel" >> "$snap"
    done < <(find "$TARGET_DIR" -type f \( -name '*.o' -o -name '*.obj' \) -path '*/build/*/out/*' -print0)
    sort -k2 -o "$snap" "$snap"
    printf '%s\n' "$snap"
}

build() {
    env "${CARGO_ENV[@]}" cargo build --release --target "$TARGET_TRIPLE" "${PKG_ARGS[@]}"
}

clean() {
    env "${CARGO_ENV[@]}" cargo clean --release --target "$TARGET_TRIPLE" "${PKG_ARGS[@]}"
}

build
first_snapshot=$(snapshot first)
object_count=$(wc -l < "$first_snapshot")
if [[ "$object_count" -eq 0 ]]; then
    echo "native-object-determinism: found 0 .o/.obj files under ${TARGET_DIR} after the first build — cc-rs output layout may have changed; this guard needs updating, not silently passing." >&2
    exit 1
fi

clean
build
second_snapshot=$(snapshot second)

if diff_out=$(diff "$first_snapshot" "$second_snapshot"); then
    echo "native-object-determinism: ${object_count} C object(s) byte-stable across 2 clang-cl rebuilds."
else
    echo "native-object-determinism: FAIL — normalized .obj/.o hash drift between clang-cl rebuilds (TimeDateStamp already excluded):" >&2
    echo "$diff_out" >&2
    exit 1
fi
