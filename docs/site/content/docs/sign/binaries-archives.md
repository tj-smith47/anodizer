+++
title = "Binaries & Archives"
description = "Sign artifacts with GPG or cosign"
weight = 1
template = "docs.html"
+++

Anodizer can sign your release artifacts using GPG or cosign.

## Minimal config

```yaml
signs:
  - artifacts: all
    cmd: gpg
    args: ["--batch", "--local-user", "{{ Env.GPG_KEY_ID }}", "--output", "${signature}", "--detach-sig", "${artifact}"]
```

## Sign config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | none | Identifier for this signing config |
| `artifacts` | string | `none` | What to sign — one of: `any`, `all`, `none`, `archive`, `binary`, `package`, `checksum`, `source`, `installer`, `diskimage`, `sbom`, `snap`, `macos_package`. (`any` is a synonym for `all`.) |
| `cmd` | string | `cosign` (or `gpg`) | Signing command. Defaults to `cosign`; falls back to the `git config gpg.program` value when set. |
| `args` | list | — | Arguments. Templates supported, plus six `${…}` substitution variables (see below). |
| `signature` | string | `{{ .Artifact }}.sig` | Signature output filename template. Templates and the `${…}` variables both apply. |
| `certificate` | string | none | Certificate file to embed in the signature (Cosign bundle signing). |
| `stdin` | string | none | Literal content piped to the signing command's stdin. |
| `stdin_file` | string | none | Path to a file piped to the signing command's stdin. |
| `ids` | list | none | Only sign artifacts from builds whose `id` is in this list. |
| `env` | list | none | Environment variables passed to the signing command (`KEY=VALUE` strings). |
| `output` | bool/template | `false` | Capture and log the signing command's stdout/stderr. Accepts a bool or a template (e.g. `"{{ IsSnapshot }}"`). |
| `if` | string | none | Template-conditional: skip this config when the rendered result is `false` or empty. |

### Argument substitution variables

Inside `args` (and `signature`), these six `${…}` placeholders are expanded per artifact before the command runs:

| Variable | Expands to |
|----------|-----------|
| `${artifact}` | Path to the artifact being signed. |
| `${signature}` | Resolved signature output path. |
| `${certificate}` | Path from the `certificate` field (empty when unset). |
| `${digest}` | The artifact's `sha256:…` digest (from metadata; empty when absent). |
| `${artifactName}` | Basename of the artifact. |
| `${artifactID}` | The producing build's `id` (empty when unset). |

## Cosign example

```yaml
signs:
  - artifacts: checksum
    cmd: cosign
    args: ["sign-blob", "--key=cosign.key", "--output-signature=${signature}", "${artifact}"]
```

## Multiple signing configs

```yaml
signs:
  - id: gpg
    artifacts: archive
    cmd: gpg
    args: ["--batch", "--detach-sig", "--output", "${signature}", "${artifact}"]
  - id: cosign
    artifacts: checksum
    cmd: cosign
    args: ["sign-blob", "--key=cosign.key", "--output-signature=${signature}", "${artifact}"]
```

## Execution & resilience

Signing commands run one subprocess per artifact, in parallel, bounded by
`--parallelism`. Two cosign-specific behaviors keep fresh CI runners from
flaking:

- **Cold-cache warm-up** — a keyless cosign config (no `--key` argument) signs
  its *first* artifact serially before fanning out. cosign initializes the
  `~/.sigstore` TUF trust root under an exclusive lock on its first run per
  host; parallel first-wave workers would race that lock and lose with
  `creating cached local store: resource temporarily unavailable`.
- **Transient retry** — failed cosign invocations are retried up to 5 attempts
  with jittered exponential backoff (2s base, 15s cap; ~29s total spread),
  since cosign depends on network infrastructure (Fulcio, Rekor, the TUF CDN).
  A missing cosign binary fails immediately. Local signers (gpg, osslsigncode,
  signtool) are deterministic and fail fast with no retry.
