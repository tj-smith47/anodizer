+++
title = "Version Files"
description = "Keep repo-committed files that embed the version in sync with the release tag"
weight = 11
template = "docs.html"
+++

`version_files` is a list of repo-committed files that embed the project
version outside `Cargo.toml` — a Helm `Chart.yaml`, an install doc, a README
badge. anodizer rewrites their version string at tag time so they never drift
from the tag, and guards them in CI.

## Minimal config

```yaml
version_files:
  - charts/myapp/Chart.yaml
  - docs/install.md
  - README.md
```

Each entry is a repo-relative path. The key is settable at the top level (as
above), under `defaults:`, or per-crate under a `crates:` entry. Precedence is
crate → `defaults` → top-level: a crate that lists its own `version_files`
overrides the shared list; a crate that lists none inherits it.

## Tag-time rewriting (`anodizer tag`)

When [`anodizer tag`](@/docs/advanced/auto-tagging.md) bumps the version, it
rewrites each enrolled file's version string from the old release version to the
new one **in the same commit that bumps `Cargo.toml` / `Cargo.lock`**, so the
files are tagged together and never drift from the tag.

Given the latest tag `v0.1.0`, a `minor` bump, and this `Chart.yaml`:

```yaml
# charts/myapp/Chart.yaml — before
version: 0.1.0
appVersion: v0.1.0
```

`anodizer tag` rewrites it in the bump commit:

```yaml
# charts/myapp/Chart.yaml — after
version: 0.2.0
appVersion: v0.2.0
```

Matching details:

- **Bare and `v`-prefixed forms both match.** `0.1.0` and `v0.1.0` are each
  rewritten to their bumped spelling (`0.2.0` / `v0.2.0`).
- **Word-boundary anchored.** `0.1.0` does not match inside `10.1.0`, so an
  unrelated longer version on the same line is left untouched.
- **A zero-match file is warned, not failed.** An enrolled file that does not
  contain the old version (usually a stale enrollment) produces a warning and
  the tag run continues:

  ```text
  $ anodizer tag
  ...
  Warning: version_files: enrolled file docs/install.md did not contain version 0.1.0 (nothing rewritten)
  ```

Pass `--dry-run` to preview the rewrite counts without writing any file.

## CI drift guard (`anodizer check version-files`)

`anodizer check version-files` is a read-only guard for CI. For each configured
crate it resolves that crate's current declared version and verifies every
enrolled file still contains it (bare or `v`-prefixed). A file whose version has
drifted — or that is missing — is reported as `STALE:` and the command exits
non-zero, so CI fails before a release goes out:

```text
$ anodizer check version-files
STALE: charts/myapp/Chart.yaml (expected 0.2.0, not found)
Error: version_files check failed with 1 finding(s)
$ echo $?
1
```

When everything is in sync the command exits 0:

```text
$ anodizer check version-files
all 3 version_files are in sync
```

When no crate enrolls any `version_files`, the guard is a no-op and exits 0 with
a short note (`no version_files configured`). Wire it into CI as a pre-release
gate:

```yaml
- name: Check version files
  run: anodizer check version-files
```

## Enrolling files (`anodizer init --version-files`)

`anodizer init --version-files` discovers tracked files that contain the current
version and enrolls your selection into `version_files` in an existing
`.anodizer.yaml`. It scans every version in play (single-crate, the shared
lockstep version, and each member's own version) and presents a scrollable
multi-select:

```text
$ anodizer init --version-files
Select files to enroll under version_files (space toggles, enter confirms)
  [x] charts/myapp/Chart.yaml
  [x] docs/install.md
  [ ] CONTRIBUTING.md
enrolled 2 file(s) under version_files in .anodizer.yaml
  + charts/myapp/Chart.yaml
  + docs/install.md
```

`Cargo.toml`, `Cargo.lock`, and `dist/` are auto-excluded — the `tag` command
already bumps the manifest and lockfile, and `dist/` is build output. Use
`--exclude <glob>` to drop further candidates from discovery (repeatable or
comma-separated):

```bash
anodizer init --version-files --exclude 'docs/**' --exclude CONTRIBUTING.md
```

Pass `-y` / `--yes` to enroll every discovered candidate without prompting —
useful in scripts:

```bash
anodizer init --version-files -y
```

Enrollment is idempotent (already-enrolled paths are never re-added) and
preserves the existing comments and key order in `.anodizer.yaml`.

## Config modes

`version_files` works in all three config modes:

- **Single-crate** — one version; enrolled files are checked and rewritten
  against the crate's own `[package].version`.
- **Workspace lockstep** — a shared version; the top-level `version_files`
  enrollment is checked against the inherited `[workspace.package].version`.
- **Workspace per-crate** — each crate enrolls its own files (under its
  `crates:` entry) and they are checked and rewritten against that crate's own
  version:

```yaml
crates:
  - name: myapp-core
    path: crates/core
    version_files:
      - crates/core/README.md
  - name: myapp-cli
    path: crates/cli
    version_files:
      - charts/myapp/Chart.yaml
      - docs/cli-install.md
```

Here `myapp-core`'s README is synced to the core crate's version while the
chart and install doc track the CLI crate's version — independently.

## A note on matching

Rewriting replaces word-boundary occurrences of the old version literal. An
unrelated line that coincidentally carries the same version string — say a
documented minimum-dependency version that happens to equal the release version
— would also be rewritten. Keep enrolled files focused on the lines that should
track the release, and run `anodizer check version-files` in CI as the safety
net.
