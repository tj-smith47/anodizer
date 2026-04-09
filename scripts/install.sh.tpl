#!/bin/sh
# Anodize remote installer — download and install the correct binary.
# Usage: curl -sSfL https://github.com/tj-smith47/anodize/releases/latest/download/install.sh | sh
set -e

REPO="tj-smith47/anodize"
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

if [ "$OS" = "windows" ]; then
    EXT="zip"
    ARCHIVE="${PROJECT}-${VERSION}-${OS}-${ARCH}.${EXT}"
else
    EXT="tar.gz"
    ARCHIVE="${PROJECT}-${VERSION}-${OS}-${ARCH}.${EXT}"
fi

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
if [ "$EXT" = "tar.gz" ]; then
    tar -xzf "${TMPDIR}/${ARCHIVE}" -C "$TMPDIR"
else
    unzip -qo "${TMPDIR}/${ARCHIVE}" -d "$TMPDIR"
fi

mkdir -p "$BINDIR"
install -m 0755 "${TMPDIR}/${PROJECT}" "${BINDIR}/${PROJECT}" 2>/dev/null \
    || cp "${TMPDIR}/${PROJECT}" "${BINDIR}/${PROJECT}"

echo "Installed ${PROJECT} v${VERSION} to ${BINDIR}/${PROJECT}"
