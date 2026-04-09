#!/bin/sh
# Anodize installer — used by makeself self-extracting archive.
# Copies the anodize binary to PREFIX/bin (default: /usr/local).
set -e

PREFIX="${PREFIX:-/usr/local}"
BINDIR="${PREFIX}/bin"

if [ ! -d "$BINDIR" ]; then
    mkdir -p "$BINDIR"
fi

if [ -f anodize ]; then
    install -m 0755 anodize "$BINDIR/anodize"
    echo "Installed anodize to $BINDIR/anodize"
else
    echo "Error: anodize binary not found in archive" >&2
    exit 1
fi
