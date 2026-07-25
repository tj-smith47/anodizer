+++
title = "Environment Preflight"
description = "Config-derived environment checks that run before any release stage"
weight = 12
template = "docs.html"
+++

Anodizer derives everything the configured release needs from the resolved
config — required CLI tools, env vars and secrets, endpoint reachability,
docker daemon availability, and loadable key material — and verifies all of
it **before any stage runs**. There is nothing to configure: requirements
are declared next to each stage and publisher implementation, so the check
surface cannot drift from what the pipeline actually reads.

## Inside `anodizer release`

The preflight runs automatically at the start of `anodizer release` and
`anodizer release --publish-only` (scoped to the stages that mode runs).
Every failure is collected in one pass and the release aborts before any
side effect:

```text
• preflight: 4 of 24 check(s) failed:
•   ✗ required tool 'cosign' not found on PATH [needed by: stage:sign, stage:docker-sign]
•   ✗ env var(s) missing or empty: COSIGN_KEY [needed by: stage:sign, stage:docker-sign]
•   ✗ env var AUR_SSH_KEY does not hold a usable SSH private key: missing trailing newline after end marker [needed by: publish:aur]
•   ✗ endpoint 'http://minio.svc:9003' unreachable: connection refused [needed by: stage:blob]
Error: preflight: 4 environment failure(s) across 24 check(s); fix the issues above before re-running
```

Secret **values** are never printed — only env-var names. Key material
(SSH, PGP, cosign) is structurally parsed, not just checked for presence,
so the classic "key works locally but the CI secret lost its trailing
newline" failure is caught before a publisher half-runs.

Snapshot and dry-run invocations skip the preflight (no upstream side
effects to guard); `--split` skips it because split legs are
operator-orchestrated partial pipelines. `--announce-only` runs a
preflight scoped to the announce stage's requirements: announcers fire
sequentially with real side effects, so a missing token aborts before the
first post instead of after half the channels are notified.

## Publisher-state report

Alongside the environment preflight above, `anodizer release` also queries
each one-way-door publisher (cargo, chocolatey, winget, aur) for the target
version's current upstream state and prints a report before publishing
starts:

```text
• Pre-flight publisher check
• cargo mycrate@1.2.3       clean
• chocolatey mycrate@1.2.3  in-moderation — package in moderation queue
• winget mycrate@1.2.3      pr-pending — https://github.com/microsoft/winget-pkgs/pull/123
• aur mycrate@1.2.3         unknown — AUR RPC returned 503
```

A row already live upstream renders under the success marker
(`✓ cargo mycrate@1.2.3  published`); every other state is a plain `•`
status line.

This report is **informational, not a gate** — none of the five states
abort the release:

| State | Meaning |
|---|---|
| `clean` | Version not present upstream; safe to publish |
| `published` | Version already published / approved; the publisher's `reconcile()` skips it (idempotent) |
| `in-moderation` | Submitted, awaiting review; `reconcile()` treats a still-pending submission as already-done work and skips |
| `pr-pending` | A manifest PR is already open for this version; `reconcile()` finds it and skips re-submitting |
| `unknown` | The state query itself failed (network error, unexpected response); `reconcile()` falls through and lets the publisher run |

Each publisher's own `reconcile()` step makes the skip-vs-dispatch call
from this same state at the moment it actually runs, so a re-run of an
in-flight release converges instead of erroring on work that is already
underway. `--preflight` runs this report and exits before publishing;
without it, the report prints and the pipeline continues regardless of
what it found.

## Standalone command

The same engine is exposed as a command — useful as a CI canary or a local
"can this machine cut the release?" check:

```bash
$ anodizer preflight                    # full pipeline surface
$ anodizer preflight --publish-only     # only what `release --publish-only` runs
$ anodizer preflight --json             # machine-readable report
$ anodizer preflight --skip=docker,blob # same stage names as release --skip
```

It reports on two independent axes: whether this runner **can** publish
(the environment report above) and whether the target version is **already**
published (the reconcile table below). The table calls the same
`reconcile()` each publisher runs at dispatch time — over the same
`--publishers` / `--skip` selection the publish loop applies, so a
deselected publisher is never probed and never gates the exit code — and so
the canary and the release cannot answer differently:

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

#### When the table is skipped

The question "is THIS version already upstream with THESE bytes?" only means
something while the resolved version is the version this run would publish.
Run `preflight` **between** two releases — the pre-tag CI canary, or a local
check on a branch with commits since the last tag — and the resolved tag is
the *last released* one, so every probe would describe a version nobody is
about to publish. anodizer locates that tag relative to `HEAD` and skips the
whole sweep in that case — a purely local git query, no network:

```text
• Reconcile state
•   skipped — v0.22.2 is already released and HEAD has advanced past it; this tree will cut a new version
```

| Tag for the resolved version | Behaviour |
|---|---|
| declared by an override, at **any** position | probe — the operator named the target version |
| does not exist | probe — a fresh version, nothing can be upstream yet |
| exists, points **at HEAD** | probe — the resume / backfill / `--publish-only` case, where a required `diverged` must still gate |
| exists, **behind** HEAD | skip — HEAD has advanced past it; a higher version will be cut |
| exists, **off HEAD's history** (older checkout, divergent branch) | skip — this tree will not publish that version |

The skip is an inference about a tag anodizer picked for you, so it never
applies to one you named. When `ANODIZER_CURRENT_TAG` (or its
`GORELEASER_CURRENT_TAG` alias, or a tag-push `GITHUB_REF_NAME`) declares the
target version, the sweep **always** runs regardless of where that tag sits
relative to `HEAD` — a backfill canary is run from a tree checked out well
past the version it is publishing, and its whole purpose is to probe that
version:

```bash
# Probes v0.20.0 even though HEAD is three releases ahead of it.
$ ANODIZER_CURRENT_TAG=v0.20.0 anodizer preflight --publish-only
```

`complete` is deliberately not an error: it is the green light a resumed
release wants. `unknown` is deliberately not an error either — an
unreachable registry must not veto a release, and the registry's own
conflict handling is the backstop. A `diverged` **optional** publisher is
reported as a warning rather than an error, because the release itself
tolerates it too — the canary is never stricter than the pipeline it guards.

### Exit codes

| Condition | Exit |
|---|---|
| Everything present, no divergence | `0` |
| Any environment requirement missing | non-zero |
| A **required** publisher `diverged` | non-zero |
| An **optional** publisher `diverged` | `0` (warning) |
| Publishers `complete` / `unknown` only | `0` |

> **Contract change.** `anodizer preflight` previously exited non-zero when a
> publisher was in moderation or had a manifest PR open. It no longer does:
> those are `complete`, the expected state of a resumed release, and treating
> them as failures is what wedged partially-failed releases. CI scripts that
> read "non-zero == do not publish" now only trip on a genuine content
> divergence or a missing credential. To act on the old signal, read the
> `--json` `reconcile[].state` field instead of the exit code.

The JSON report carries a `kind` per environment failure (`missing_tool`,
`missing_env`, `endpoint_unreachable`, `docker_unavailable`,
`bad_key_material`) alongside a `reconcile` array — one object per
publisher with `publisher`, `state`, `detail`, and `blocking`:

```json
{
  "checks": 24,
  "failures": [],
  "reconcile": [
    { "publisher": "cargo", "state": "complete", "detail": "1.2.3 live with matching cksum", "blocking": false },
    { "publisher": "npm", "state": "absent", "blocking": false }
  ]
}
```

A **skipped** sweep projects to one marker row rather than to `[]`, so
"this question did not apply" can never be read as "no publisher is
configured". Its `publisher` is the whole-set wildcard `*`:

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

## Secrets-only pre-tag gate

Decoupled CI pipelines split a release across many runners — build and
determinism shards on different hosts, plus a dedicated publish runner —
that carry **different host-local tools** but the **same injected
secrets** (CI secrets are exported into every job). For those pipelines, a
single up-front gate should answer "are all the publish credentials
present and well-formed?" *without* false-failing on a tool that only the
eventual publish host has.

`anodizer release --preflight-secrets` is that gate. It collects the full
release surface, then keeps only the **runner-agnostic credential**
requirements — env vars and env-borne key material (`COSIGN_KEY`,
`GITHUB_TOKEN` ladders, `env://` SSH/PGP keys, …) — and drops every
host-local requirement (CLI tools, the docker daemon, endpoint
reachability, and on-disk key *files*, which may not be materialized on
the gate runner). Env-borne key material is still structurally validated,
so a malformed secret key is caught before a tag is minted; on-disk key
*files* are not checked by this gate. The check runs zero mutations: no
`before:` hooks, no network probes, no pipeline.

```bash
$ anodizer release --preflight-secrets
• preflight-secrets: all required publish secrets / credentials present
```

Wire it as the **root job a release depends on**, so a missing CI secret
aborts before the tag is created (and before the expensive determinism
matrix runs):

```yaml
jobs:
  preflight:
    runs-on: ubuntu-latest
    permissions:
      id-token: write   # so OIDC-provenance request vars are present to check
    steps:
      - uses: actions/checkout@v6
      - uses: ./.github/actions/setup-rust
      # Build from the checkout (not a published anodizer) so a brand-new
      # release that adds a publisher's secret is gated by THIS commit.
      # --skip=<stage> any stage whose secret is RUNNER-AMBIENT rather than a
      # registered CI secret (e.g. a self-hosted runner's ambient cloud creds):
      # this github-hosted gate cannot see those, so demanding them here would
      # false-fail and block every release. They are validated in-pipeline on
      # the runner that holds them.
      - run: cargo run -q --release -p anodizer -- release --preflight-secrets --skip=blob
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
          COSIGN_KEY: ${{ secrets.COSIGN_KEY }}
          NPM_TOKEN: ${{ secrets.NPM_TOKEN }}
          # …every secret the downstream publish jobs consume as REGISTERED CI
          # secrets (exclude runner-ambient ones via --skip above)…

  tag:
    needs: [preflight]
    if: needs.preflight.result == 'success'
    # …auto-tag only once the secret gate is green…
```

The gate runs even when HEAD carries no release tag (it is a *pre-tag*
check) and ignores dirty-tree / dist state, since it never reads or writes
either.

## What gets derived

| Surface | Derived requirements |
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
