#!/bin/sh
# Anodizer remote installer — download and install the correct binary.
# Usage: curl -sSfL https://github.com/tj-smith47/anodizer/releases/latest/download/install.sh | sh
set -e

REPO="tj-smith47/anodizer"
VERSION="{{ Version }}"
PROJECT="{{ ProjectName }}"
PREFIX="${PREFIX:-/usr/local}"
BINDIR="${BINDIR:-${PREFIX}/bin}"

detect_os() {
    case "$(uname -s)" in
        Linux*)  echo "linux" ;;
        Darwin*) echo "darwin" ;;
        MINGW*|MSYS*|CYGWIN*) echo "windows" ;;
        *) echo "unsupported" ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)  echo "amd64" ;;
        aarch64|arm64) echo "arm64" ;;
        *) echo "unsupported" ;;
    esac
}

OS="$(detect_os)"
ARCH="$(detect_arch)"

if [ "$OS" = "unsupported" ] || [ "$ARCH" = "unsupported" ]; then
    echo "Error: unsupported platform $(uname -s)/$(uname -m)" >&2
    exit 1
fi

# Asset filenames are emitted by anodizer from this release's archive
# name_template + format_overrides, so each URL resolves to a real asset —
# no shell-hardcoded name that 404s the moment the template or a format
# override changes.
case "${OS}-${ARCH}" in
{{ InstallerAssetCases }}
    *)
        echo "Error: no prebuilt ${PROJECT} binary for ${OS}/${ARCH}" >&2
        exit 1
        ;;
esac

URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ARCHIVE}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading ${PROJECT} v${VERSION} for ${OS}/${ARCH}..."
if command -v curl > /dev/null 2>&1; then
    curl -sSfL "$URL" -o "${TMPDIR}/${ARCHIVE}"
elif command -v wget > /dev/null 2>&1; then
    wget -qO "${TMPDIR}/${ARCHIVE}" "$URL"
else
    echo "Error: curl or wget required" >&2
    exit 1
fi

echo "Extracting..."
# Extraction follows the asset's own suffix, so it stays correct whatever
# format the archive name carries (zip on windows, tar.gz elsewhere).
case "$ARCHIVE" in
    *.zip)
        unzip -qo "${TMPDIR}/${ARCHIVE}" -d "$TMPDIR"
        ;;
    *)
        tar -xzf "${TMPDIR}/${ARCHIVE}" -C "$TMPDIR"
        ;;
esac

mkdir -p "$BINDIR"
install -m 0755 "${TMPDIR}/${PROJECT}" "${BINDIR}/${PROJECT}" 2>/dev/null \
    || cp "${TMPDIR}/${PROJECT}" "${BINDIR}/${PROJECT}"

echo "Installed ${PROJECT} v${VERSION} to ${BINDIR}/${PROJECT}"
