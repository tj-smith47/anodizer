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

Snapshot, dry-run, split, and announce-only invocations skip the
preflight (no side effects to guard); `--no-preflight` overrides it
explicitly.

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

## What gets derived

| Surface | Derived requirements |
|---------|---------------------|
| `builds` | `cargo` (honors `$CARGO`) |
| `nfpms` / `srpms` | `nfpm` / `rpmbuild` + signing key material from `signature:` blocks |
| `snapcrafts` | `snapcraft`, `unsquashfs`; `SNAPCRAFT_STORE_CREDENTIALS` when `publish: true` |
| `signs` / `binary_signs` / `docker_signs` | the signing `cmd`, env refs in args/stdin, `env://VAR` cosign keys validated as key material |
| `sboms` / `makeselfs` / `appimages` / `upx` | `syft` (or custom `cmd`), `makeself`, `linuxdeploy`, `upx` |
| `dockers_v2` | `docker` + reachable daemon |
| `blobs` | rendered S3 `endpoint` reachability, static keypair for custom endpoints, KMS CLIs |
| `verify_release.install_smoke` | `docker` + reachable daemon |
| publishers | per-publisher token ladders (e.g. `HOMEBREW_TAP_TOKEN` → `GITHUB_TOKEN`), per-entry secret env names, AUR SSH keys |

Entries disabled via `skip:` / `skip_upload:` / a falsy `if:` contribute
nothing, and in per-crate workspace mode the requirements are the union
across every publishable crate — one preflight covers the whole run.
