+++
title = "Version Files"
description = "Keep repo-committed files that embed the version in sync with the manifest version"
weight = 11
template = "docs.html"
+++

`version_files` is a list of repo-committed files that embed the project
version outside `Cargo.toml` — a Helm `Chart.yaml`, an install doc, a README
badge. anodizer rewrites their version string at tag time so they never drift
from the manifest, and guards them in CI.

## Minimal config

```yaml
version_files:
  - docs/installation.md                       # bare: whole-file literal sweep
  - path: chart/cfgd/values.yaml
    match: 'operator:\s+image:.*:v{version}'   # regex; {version} = this crate's version
```

An entry is either a bare repo-relative path (a whole-file sweep) or a `path` +
`match` mapping that scopes the rewrite to the occurrences its regex selects —
see [Scoping an entry to one occurrence](#scoping-an-entry-to-one-occurrence-match).
A bare list stays exactly as it always was:

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
new one **in the same commit that bumps `Cargo.toml` / `Cargo.lock`**. The
enrolled files follow the manifest write, so they never drift from the manifest:
whenever the bump commit writes `Cargo.toml`, it writes them too, and when it
does not — `version_sync` off for the only declared crate — it leaves both alone
and `check version-files` reports them stale until you sync them. Pass
`--changelog` and that bump commit also
[refreshes `CHANGELOG.md`](@/docs/advanced/auto-tagging.md#refreshing-changelog-md-changelog)
(opt-in; requires a `changelog:` block) — both ride the same commit.

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
       Error STALE: charts/myapp/Chart.yaml (expected 0.2.0, not found)
       Error version_files check failed with 1 finding(s)
$ echo $?
1
```

When everything is in sync the command exits 0:

```text
$ anodizer check version-files
   • all 3 version_files are in sync
```

An anchored entry is verified INSIDE its anchor: the guard compiles the `match`
regex against the crate's current version and reports
`STALE: <file> (match <anchor>: expected <version>, not found)` when that anchor
selects nothing — so a version present elsewhere in the file does not mask the
drift. A `match` that omits `{version}` or is not a valid regex is reported as a
finding rather than crashing the guard.

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
multi-select — space toggles a candidate, enter confirms. Once confirmed (or
with `-y`, which selects every candidate) it reports what it enrolled:

```text
$ anodizer init --version-files -y
   • enrolled 2 file(s) under version_files in .anodizer.yaml
   •   + charts/myapp/Chart.yaml
   •   + docs/install.md
```

`Cargo.toml`, `Cargo.lock`, and `dist/` are auto-excluded — the `tag` command
already bumps the manifest and lockfile, and `dist/` is build output. Use
`--exclude <glob>` to drop further candidates from discovery (repeatable or
comma-separated):

```bash
anodizer init --version-files --exclude 'docs/**' --exclude CONTRIBUTING.md
```

`-y` / `--yes` (used above) skips the prompt and enrolls every discovered
candidate — useful in scripts.

Enrollment is idempotent (already-enrolled paths are never re-added) and
preserves the existing comments and key order in `.anodizer.yaml`. Discovery has
no basis to infer an anchor, so `init` only ever writes bare `- <path>` items —
but it reads both forms, so a file already enrolled with a `match` anchor is
never re-offered, and a new item lands after the anchored mapping, not inside
it.

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

## Scoping an entry to one occurrence (`match`)

A file that carries two crates' versions — a Helm values file pinning two
images, an install doc showing two sample commands — cannot be enrolled bare by
both crates: one whole-file sweep would rewrite the other crate's lines. Give
each enrollment a `match` anchor and each rewrites only its own occurrences,
even when both crates sit at the same literal version.

```yaml
# chart/cfgd/values.yaml — two crates, one literal
operator:
  image: ghcr.io/tj-smith47/cfgd-operator:v0.7.0
csi:
  image: ghcr.io/tj-smith47/cfgd-csi:v0.7.0
```

```yaml
crates:
  - name: cfgd-operator
    path: crates/operator
    version_files:
      - path: chart/cfgd/values.yaml
        match: 'operator:\s+image:.*:v{version}'
  - name: cfgd-csi
    path: crates/csi
    version_files:
      - path: chart/cfgd/values.yaml
        match: 'csi:\s+image:.*:v{version}'
```

A minor bump of the operator and a patch bump of the CSI driver rewrite one pin
each:

```yaml
# chart/cfgd/values.yaml — after
operator:
  image: ghcr.io/tj-smith47/cfgd-operator:v0.8.0
csi:
  image: ghcr.io/tj-smith47/cfgd-csi:v0.7.1
```

The rules:

| Rule | Detail |
|---|---|
| `{version}` | Stands for the OLD version — the one the file currently carries — regex-escaped before matching, so its `.` separators are literal. Only the literal 9-character token `{version}` is substituted, so regex quantifiers like `\d{2}` are untouched. The anchor is never re-rendered with the new version: it exists to FIND the region, and the replacement happens inside it. |
| Required | `match` must contain `{version}` at least once. An anchor without it cannot be verified and is refused. |
| The rewrite | Inside a match, the replacement is the SAME word-boundary literal replace the bare form uses — bare and `v`-prefixed spellings both. `match` selects the region; it never supplies the new text, and the surrounding text the anchor matched is left byte-for-byte alone. |
| Every match | All regions the anchor selects are rewritten, not just the first. |
| Zero matches | An **error** that fails the tag before any file is written (unlike a bare entry's warning). An anchor states a precise intent, so a silent no-op is a defect, not a nuisance. |
| Two anchors | Two anchors on one file must select disjoint regions; anodizer does not check that they do. |

```text
$ anodizer tag
       Error version_files: crate 'cfgd-csi' enrolled chart/cfgd/values.yaml with match "csi:\\s+image:.*:v{version}" but it matched nothing (expected version 0.7.0); fix the anchor or remove the enrollment
```

## Sharing one file between crates

Two crates may enroll the same file. Which pairings are legal is decided before
anything is written, identically in `--dry-run` and a real run:

- **Distinct old versions, no chain** — both rewrite. `cfgd 0.9.0 → 0.10.0`
  beside `cfgd-operator 0.7.0 → 0.8.0` in one `docs/installation.md` is fine:
  each pair only matches its own literal.
- **The same old version bumped to two different new ones** — refused. One
  literal cannot become two versions; scope each side with a `match` anchor
  instead.

  ```text
  version_files conflict: chart/cfgd/values.yaml is enrolled by crates bumping FROM the same version to different versions (cfgd-operator 0.7.0 → 0.8.0 vs cfgd-csi 0.7.0 → 0.7.1); a file cannot hold two new versions for one old one
  ```

- **A chain** — refused. When one crate's NEW version is still matched by
  another's OLD matcher, the second rewrite consumes the first's output and no
  apply order fixes it.

  ```text
  version_files conflict: shared.md is enrolled by crates whose bumps chain (core 0.1.0 → 0.2.0 then cli 0.2.0 → 0.3.0); the second rewrite would consume the first's output — give each enrollment its own `match` anchor
  ```

  The chain test is on the matcher, not on string equality, so a prerelease
  beside its release base is a chain too: `0.1.0-rc1 → 0.1.0-rc2` beside
  `0.1.0 → 0.2.0` is refused, because the word-boundary matcher for `0.1.0`
  fires inside `0.1.0-rc2` and would corrupt it to `0.2.0-rc2`. Give each
  enrollment its own `match` anchor.

A **bare** entry sweeps the whole file, so it overlaps every anchored region in
it. That pairing is judged by the same two hazards, and the message names the
overlap (`whole-file entry overlaps match …`) — a bare entry bumping `0.7.0 →
0.8.0` beside an anchored one bumping `0.7.0 → 0.7.1` is refused, and so is a
chain between them. A bare and an anchored entry on the SAME `old → new` bump
are legal and both apply: each occurrence is rewritten exactly once, because
every anchored entry claims its regions in the original file before the bare
sweep sees what is left. Anchoring every enrollment of a shared file still
documents the intent best.

## A note on matching

Rewriting replaces word-boundary occurrences of the old version literal. An
unrelated line that coincidentally carries the same version string — say a
documented minimum-dependency version that happens to equal the release version
— would also be rewritten. A `match` anchor is the fix: scope the enrollment to
the lines that should track the release. Keep enrolled files focused, and run
`anodizer check version-files` in CI as the safety net.
