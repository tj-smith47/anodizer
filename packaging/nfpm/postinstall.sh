#!/bin/sh
# Refresh the system man-page index after install so `man anodizer` and
# `apropos anodizer` resolve immediately (anodizer ships /usr/share/man/man1/
# anodizer.1). Best-effort and portable across the package targets: Debian/RPM
# distros ship man-db's `mandb`; Alpine (apk) ships mandoc's `makewhatis`. A
# host with neither — or a failing refresh — is non-fatal: the page is already
# installed, only its index is stale.
set -e

if command -v mandb >/dev/null 2>&1; then
	mandb --quiet >/dev/null 2>&1 || true
elif command -v makewhatis >/dev/null 2>&1; then
	makewhatis /usr/share/man >/dev/null 2>&1 || true
fi

exit 0
