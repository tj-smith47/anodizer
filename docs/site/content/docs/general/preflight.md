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
Error: preflight: 4 environment failure(s) across 24 check(s); fix the issues above or re-run with --no-preflight to override
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
`--no-preflight` overrides it explicitly.

## Standalone command

The same engine is exposed as a command — useful as a CI canary or a local
"can this machine cut the release?" check:

```bash
$ anodizer preflight                    # full pipeline surface
$ anodizer preflight --publish-only     # only what `release --publish-only` runs
$ anodizer preflight --json             # machine-readable report
$ anodizer preflight --skip=docker,blob # same stage names as release --skip
```

The exit code is non-zero when anything is missing, and the JSON report
carries a `kind` per failure (`missing_tool`, `missing_env`,
`endpoint_unreachable`, `docker_unavailable`, `bad_key_material`).

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

`--preflight-secrets` and `--no-preflight` are mutually exclusive. The
gate runs even when HEAD carries no release tag (it is a *pre-tag* check)
and ignores dirty-tree / dist state, since it never reads or writes
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
