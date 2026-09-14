+++
title = "Preflight"
description = "The one preflight engine: environment, publisher credentials and one-way-door state, and the reconcile sweep, run before any release stage"
weight = 12
template = "docs.html"
+++

Anodizer answers three questions about a tree before it releases anything,
with zero mutations:

1. **Can this runner publish?** Every enabled stage and publisher declares
   what it needs — CLI tools, env vars and secrets, endpoint reachability,
   the docker daemon, loadable key material — and all of it is evaluated in
   one collect-all pass.
2. **Will every publisher accept the target version?** Each one-way-door
   publisher (cargo, chocolatey, winget, aur) reports the version's upstream
   state, each publisher probes its own credential, and the rollback scope
   each publisher would need is checked.
3. **Is the target version already upstream with these bytes?** Each selected
   publisher runs the same `reconcile()` it runs at dispatch time.

One engine asks all three, in that order, and `anodizer preflight` and
`anodizer release` both run it. There is nothing to configure: requirements
are declared next to each stage and publisher implementation, so the check
cannot drift from what the pipeline reads.

## Inside `anodizer release`

The engine runs once, after the config and git context resolve and before the
`before:` hooks, in `anodizer release` and `anodizer release --publish-only`
(scoped to the stages that mode runs). Every failure is collected in one pass
and the release aborts before any side effect:

```text
       Error 4 of 24 preflight check(s) failed:
       Error   ✗ tool 'cosign' not found on PATH [needed by: stage:sign, stage:docker-sign]
       Error   ✗ env var(s) missing or empty: COSIGN_KEY [needed by: stage:sign, stage:docker-sign]
       Error   ✗ env var AUR_SSH_KEY does not hold a usable SSH private key: missing '-----END ... PRIVATE KEY-----' footer [needed by: publish:aur]
       Error   ✗ endpoint 'http://minio.svc:9003' unreachable: connection refused [needed by: stage:blob]
       Error preflight: 4 environment failure(s) across 24 check(s); fix the issues above before re-running
```

Secret **values** are never printed — only env-var names. Key material
(SSH, PGP, cosign) is structurally parsed as well as checked for presence,
so the classic "the CI secret pasted in truncated" failure is caught before
a publisher half-runs. A key that merely lost its trailing newline is
accepted: the key writer normalizes that before ssh reads it.

The release skips the engine in four cases:

| Invocation | Why |
|---|---|
| `release --skip=preflight` | a pre-tag CI job already ran `anodizer preflight` on this tree (see [the CI pattern](#the-ci-pattern)) |
| `release --snapshot` | no upstream side effects to guard |
| `release --dry-run` | same |
| `release --split` | split legs are operator-orchestrated partial pipelines |

`--announce-only` runs the environment half alone, scoped to the announce
stage's requirements: announcers fire sequentially with real side effects,
so a missing token aborts before the first post. The publisher half is
also skipped whenever the `publish` stage is skipped, since neither run
crosses a one-way door.

## Publisher check

After the environment report, the engine asks each one-way-door publisher
for the target version's upstream state, and every selected publisher
probes its own credential and reports the rollback scope it would need:

```text
   • Pre-flight publisher check
   • cargo mycrate@1.2.3       clean
   • chocolatey mycrate@1.2.3  in-moderation — package in moderation queue
   • winget mycrate@1.2.3      pr-pending — https://github.com/microsoft/winget-pkgs/pull/123
   • aur mycrate@1.2.3         unknown — AUR RPC returned 503
   • preflight found 1 publisher(s) clean
```

A row already live upstream renders under the success marker
(`✓ cargo mycrate@1.2.3  published`); every other state is a plain `•`
status line. None of the five states aborts the run:

| State | Meaning |
|---|---|
| `clean` | Version not present upstream; safe to publish |
| `published` | Version already published / approved; the publisher's `reconcile()` skips it (idempotent) |
| `in-moderation` | Submitted, awaiting review; `reconcile()` treats a still-pending submission as already-done work and skips |
| `pr-pending` | A manifest PR is already open for this version; `reconcile()` finds it and skips re-submitting |
| `unknown` | The state query itself failed (network error, unexpected response); `reconcile()` falls through and lets the publisher run |

Each publisher's own `reconcile()` step makes the skip-vs-dispatch call
from this same state at the moment it runs, so a re-run of an in-flight
release converges on work that is already underway.

What can abort the run here is a **blocker**: a credential a publisher probed
and found unusable with no other way to authenticate (see the
[npm page](@/docs/publish/npm.md#preflight-severity-for-an-unusable-token)
for how one publisher grades that), or a missing rollback scope under
`--strict`. Every other finding is a **warning** that prints and lets the
run continue — a rollback scope missing in default mode, a credential probe
that could not reach a verdict, a publisher that is optional. The
[rollback scope preflight](@/docs/advanced/release-resilience.md#rollback-scope-preflight)
lists what is asked of each publisher. A blocker aborts the run with a
`preflight: N resilience blocker(s): …` line naming each one.

## Reconcile sweep

The third half calls the same `reconcile()` each publisher runs at dispatch
time, over the same `--publishers` / `--skip` selection the publish loop
applies, so a deselected publisher is never probed and never gates the exit
code, and the standalone command and the release cannot answer differently:

```text
   • Reconcile state
   ✓ cargo       complete — 1.2.3 live with matching cksum
   • npm         absent — will publish
   ✓ winget      complete — open PR https://github.com/microsoft/winget-pkgs/pull/123
   • aur         unknown — probe failed: AUR RPC returned 503
```

| State | Meaning | Blocks? |
|---|---|---|
| `absent` | Not upstream yet; the publisher will publish | no |
| `complete` | This exact version **and content** is already upstream (live, in moderation, or an open PR); the publisher skips | no |
| `diverged` | The version is upstream but the local artifact bytes differ | **yes, if the publisher is required** |
| `unknown` | The probe was inconclusive (network error, unparseable feed) | no |

The sweep asks the real hosts: the PR-mode publishers (nix, homebrew,
homebrew-core, krew, scoop, winget) search the upstream index repository's
pull requests on the GitHub API, cargo reads the crates.io sparse index,
npm and PyPI their registries, chocolatey its feed; a call that fails reads
as `unknown` on that row and never fails an otherwise clean report.

`complete` is deliberately not an error: it is the approval a resumed
release wants. `unknown` is deliberately not an error either — an
unreachable registry must not veto a release, and the registry's own
conflict handling is the backstop. A `diverged` **optional** publisher is
reported as a warning, because the release itself tolerates it too — the
standalone command is never stricter than the pipeline it guards. A
required `diverged` aborts the run and asks for a version bump: the version
is already published with different content.

## Standalone command

The same engine is exposed as a command — the pre-tag CI job, or a local
"can this machine cut the release?" check:

```bash
$ anodizer preflight                    # the whole engine, full pipeline scope
$ anodizer preflight --publish-only     # only what `release --publish-only` runs
$ anodizer preflight --json             # machine-readable report
$ anodizer preflight --skip=docker,blob # same stage names as release --skip
```

### Which version is probed

The publisher check and the reconcile sweep are only meaningful against the
version this tree would release, so the command derives it the way
`anodizer tag` does:

| Tree | Version the probes use |
|---|---|
| `ANODIZER_CURRENT_TAG` (or its `GORELEASER_CURRENT_TAG` alias, or a tag-push `GITHUB_REF_NAME`) names a tag | that tag, wherever it sits relative to `HEAD` — the operator named the target |
| `HEAD` carries the configured tag | that tag — the resume / backfill / `--publish-only` case, where a required `diverged` must still gate |
| commits since the last tag carry a release signal (`#major` / `#minor` / `#patch`, a conventional `feat:` / `fix:`, …) | the version `anodizer tag` would cut next |
| commits since the last tag carry no release signal | the current version; the reconcile sweep is skipped (below) |

Under `-v` the derivation is printed:

```text
$ anodizer preflight -v
   • HEAD is not tagged; publisher probes use the planned version 0.27.1 (v0.27.0 → v0.27.1)
```

The derivation needs the tag history, so a CI checkout that runs it passes
`fetch-depth: 0`.

#### When the reconcile sweep is skipped

With no release signal since the last tag the resolved version is the last
released one, and every probe would describe a version nobody is about to
publish. anodizer locates that tag relative to `HEAD` with a local git query
and skips the whole sweep:

```text
   • Reconcile state
   •   skipped — v0.22.2 is already released and HEAD has advanced past it; this tree will cut a new version
```

| Tag for the resolved version | Behaviour |
|---|---|
| declared by an override, at **any** position | probe — the operator named the target version |
| does not exist | probe — a fresh version (including the planned one), nothing can be upstream yet |
| exists, points **at HEAD** | probe — the resume / backfill / `--publish-only` case |
| exists, **behind** HEAD | skip — HEAD has advanced past it and nothing plans a new version |
| exists, **off HEAD's history** (older checkout, divergent branch) | skip — this tree will not publish that version |

The skip is an inference about a tag anodizer picked for you, so it never
applies to one you named — a backfill run from a tree checked out well past
the version it is publishing probes exactly that version:

```bash
# Probes v0.20.0 even though HEAD is three releases ahead of it.
$ ANODIZER_CURRENT_TAG=v0.20.0 anodizer preflight --publish-only
```

### Exit codes

| Condition | Exit |
|---|---|
| Everything present, no blocker, no divergence | `0` |
| Any environment requirement missing | non-zero |
| A publisher **blocker** (unusable sole credential; missing rollback scope under `--strict`) | non-zero |
| A **required** publisher `diverged` | non-zero |
| Publisher warnings only; an **optional** publisher `diverged` | `0` |
| Publishers `complete` / `unknown` only | `0` |

> **Contract change.** `anodizer preflight` previously exited non-zero when a
> publisher was in moderation or had a manifest PR open. It no longer does:
> those are `complete`, the expected state of a resumed release, and treating
> them as failures is what wedged partially-failed releases. CI scripts that
> read "non-zero == do not publish" now only trip on a genuine content
> divergence, a blocker, or a missing credential. To act on the old signal,
> read the `--json` `reconcile[].state` field instead of the exit code.

### JSON report

`--json` carries all three halves: the environment keys at the top level
(with a `kind` per failure — `missing_tool`, `missing_env`,
`endpoint_unreachable`, `docker_unavailable`, `bad_key_material`), a
`publishers` object with the publisher check's `entries`, `warnings` and
`blockers` (`null` when the publisher half was skipped), and a `reconcile`
array with one object per publisher:

```json
{
  "checks": 24,
  "failures": [],
  "publishers": {
    "entries": [
      { "publisher": "cargo", "package": "mycrate", "version": "1.2.3", "state": "clean" },
      { "publisher": "chocolatey", "package": "mycrate", "version": "1.2.3", "state": { "in-moderation": { "reason": "package in moderation queue" } } }
    ],
    "warnings": [],
    "blockers": []
  },
  "reconcile": [
    { "publisher": "cargo", "state": "complete", "detail": "1.2.3 live with matching cksum", "blocking": false },
    { "publisher": "npm", "state": "absent", "blocking": false }
  ]
}
```

A **skipped** sweep projects to one marker row, so "this question did not
apply" can never be read as "no publisher is configured". Its `publisher`
is the whole-set wildcard `*`:

```json
{
  "reconcile": [
    {
      "publisher": "*",
      "state": "skipped",
      "detail": "v0.22.2 is already released and HEAD has advanced past it; this tree will cut a new version",
      "blocking": false
    }
  ]
}
```

## The CI pattern

A pipeline that tags automatically runs the engine **once, before the tag
exists**, as the root job every other job depends on, and every job that
runs `anodizer release` afterwards passes `--skip=preflight`. A missing or
truncated secret, an unreachable endpoint, or a version a registry already
holds then aborts the run with nothing tagged and nothing published, and the
release jobs never spend a second network round on a question that is
already answered.

Run the job on the runner that will publish, so the endpoints it probes
are the ones the publish reaches and ambient credentials (a self-hosted
runner's cloud keys, for instance) are checked in the same pass. Where a
stage's credential is genuinely absent on the preflight runner, `--skip`
that stage there and let the publish job carry it:

```yaml
jobs:
  preflight:
    runs-on: arc-anodizer          # the runner the release job will use
    permissions:
      contents: read
      id-token: write              # so the OIDC request vars are present to check
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0           # the planned version is derived from the tag history
      - uses: tj-smith47/anodizer-action@v1
        with:
          auto-install: true
          args: preflight
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
          COSIGN_KEY: ${{ secrets.COSIGN_KEY }}
          # …every secret the release job consumes, so the two env blocks match…

  tag:
    needs: [preflight]
    if: needs.preflight.result == 'success'
    # …auto-tag only once the gate passes…

  release:
    needs: [tag]
    runs-on: arc-anodizer
    steps:
      - uses: tj-smith47/anodizer-action@v1
        with:
          args: release --publish-only --skip=preflight,npm,pypi,cargo
          # the pre-tag job already ran the engine on this tree
```

The job runs on an untagged tree by design: the probes use the version the
tag job is about to cut. anodizer's own pipeline is the worked example, in
[The Release Pipeline](@/docs/ci/release-pipeline.md).

## What gets derived

| Stage or publisher | Derived requirements |
|---------|---------------------|
| `builds` | `cargo` |
| `nfpms` / `srpms` | `nfpm` / `rpmbuild` + signing key material from `signature:` blocks |
| `snapcrafts` | `snapcraft`, `unsquashfs`; `SNAPCRAFT_STORE_CREDENTIALS` when `publish: true` |
| `signs` / `binary_signs` / `docker_signs` | the signing `cmd`, env refs in args/stdin, `env://VAR` cosign keys validated as key material |
| `sboms` / `makeselfs` / `appimages` / `upx` | `syft` (or custom `cmd`), `makeself`, `linuxdeploy`, `upx` |
| `dockers_v2` | `docker` + reachable daemon |
| `blobs` | rendered S3 `endpoint` reachability, static keypair for custom endpoints, KMS CLIs |
| `verify_release.install_smoke` | `docker` + reachable daemon |
| `msis` | `wix` (v4) or `candle` + `light` (v3) — same explicit-`version:` > `.wxs`-namespace-sniff > installed-tool-probe policy the build uses; only when a Windows target is configured |
| `nsis` | `makensis`; only when a Windows target is configured |
| `pkgs` | `pkgbuild`; only when a macOS target is configured |
| `dmgs` | any of `hdiutil` / `genisoimage` / `mkisofs` (the stage's own detection ladder); only when a macOS target is configured |
| `flatpaks` | `flatpak-builder` + `flatpak`; only when a Linux target is configured |
| `app_bundles` | nothing — the stage assembles the `.app` layout with pure file operations |
| `notarize` | `rcodesign` + env refs in `certificate:` / `password:` / API-key fields (cross-platform), `codesign` + `xcrun` + env refs in `identity:` / `keychain:` / `profile_name:` (native) |
| `announce` | per-announcer secrets exactly as the senders read them — e.g. `SLACK_WEBHOOK` (or env refs in a templated `webhook_url:`), `TELEGRAM_TOKEN`, `DISCORD_WEBHOOK_ID`+`DISCORD_WEBHOOK_TOKEN`, full Twitter/Reddit/Mastodon credential sets, and `SMTP_HOST` / `SMTP_USERNAME` / `SMTP_PASSWORD` for email (password only when encryption is enabled) |
| publishers | per-publisher token ladders (e.g. `HOMEBREW_TAP_TOKEN` → `GITHUB_TOKEN`), per-entry secret env names, AUR SSH keys |

Entries disabled via `skip:` / `skip_upload:` / a falsy `if:` contribute
nothing, and in per-crate workspace mode the requirements are the union
across every publishable crate — one preflight covers the whole run.

The per-platform bundler stages (`msis`, `nsis`, `pkgs`, `dmgs`,
`flatpaks`) contribute requirements only when the **configured build
targets** include their platform — mirroring each stage's own run gate, so
a Linux-only matrix never demands WiX. Announce requirements are checked in
both the full and `--publish-only` scopes (the publish-only pipeline runs
announce), and `--announce-only` checks them alone — the only stage that
mode runs.
