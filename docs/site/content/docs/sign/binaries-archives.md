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
| `stdin` | string | none | Content piped to the signing command's stdin; template-expanded (e.g. `{{ Env.GPG_PASSPHRASE }}`). |
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

## Signing the raw binaries

`binary_signs:` signs each built binary before it is packaged. Its `artifacts`
field accepts only `binary` (or `none`); everything else about an entry matches
a `signs:` entry.

```yaml
binary_signs:
  - artifacts: binary
    cmd: cosign
    args: ["sign-blob", "--key=cosign.key", "--output-signature=${signature}", "${artifact}"]
```

The signature and certificate upload as release assets alongside the archives.
Each is named from the crate's `archives:` **config** — never from which
archives the run happens to have built, so `anodizer build` and
`anodizer release` name the same binary's signature identically:

| The binary's target | Asset name |
|---|---|
| Covered by the crate's primary `archives:` entry — the first entry in config order whose `ids:` / `binaries:` filters take this binary | That entry's `name_template` rendered for the target, plus the suffix the `signature:` / `certificate:` template appended to the binary's file name. `app-1.2.3-linux-amd64.sig`, `app-1.2.3-windows-amd64.sig` (from `app.exe.sig`), `app-1.2.3-linux-amd64.bundle.sig` |
| Covered by no archive entry — a musl build that only feeds npm, say | `{{ Binary }}-{{ Version }}-{{ Target }}` plus that suffix: `app-1.2.3-x86_64-unknown-linux-musl.sig`. The whole triple is required, because `Os`/`Arch` render identically for a gnu and a musl build |
| Published by a `formats: [binary]` entry | That uploaded executable's own name plus the suffix, so a `signs:` config covering the asset and a `binary_signs:` config covering the same bytes resolve to one name |

The raw binary is called `app` (or `app.exe`) under every target's directory,
so its own basename would collapse every target's signature onto one asset:

```
app-1.2.3-linux-amd64.tar.gz
app-1.2.3-linux-amd64.sig          <- binary_signs
app-1.2.3-linux-arm64.tar.gz
app-1.2.3-linux-arm64.sig          <- binary_signs
app-1.2.3-windows-amd64.zip
app-1.2.3-windows-amd64.sig        <- binary_signs
app-1.2.3-linux-amd64v3.tar.gz
app-1.2.3-linux-amd64v3.sig        <- binary_signs, micro-arch variant
```

A crate with several `archives:` entries builds several archives per target;
the signature takes the name of the first entry in config order, the primary
archive that `chocolatey` and `scoop` bind to with `ids: [default]`:

```yaml
archives:
  - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"   # id: default
    formats: [tar.gz]
  - id: extra
    name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}-extra"
    formats: [tar.xz]
```

```
app-1.2.3-linux-amd64.tar.gz
app-1.2.3-linux-amd64-extra.tar.xz
app-1.2.3-linux-amd64.sig          <- binary_signs, the primary entry's name
```

### Overriding the asset name

`asset_name_template:` replaces the derived name. It renders the asset's BASE
name in the same per-target scope an archive `name_template` renders under
(`Os`, `Arch`, `Target`, the micro-architecture variants, `CrateName`,
`Binary`); the `signature:` / `certificate:` suffix still carries over, so one
template names both assets:

```yaml
binary_signs:
  - artifacts: binary
    cmd: cosign
    args: ["sign-blob", "--key=cosign.key", "--output-signature=${signature}", "${artifact}"]
    certificate: "{{ .Artifact }}.pem"
    asset_name_template: "{{ Binary }}-{{ Version }}-{{ Target }}"
```

```
app-1.2.3-x86_64-unknown-linux-gnu.sig
app-1.2.3-x86_64-unknown-linux-gnu.pem
```

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

- **Keyless cosign is serialized** — a keyless config (no `--key` argument)
  runs one invocation at a time regardless of `--parallelism`, and holds a
  host-level advisory lock (`~/.sigstore/root/.anodizer-tuf-init.lock`, or
  `$TUF_ROOT`) so a second anodizer process on the same host queues rather
  than races. Concurrent keyless cosign invocations collide on the sigstore
  TUF trust store and the loser exits with `creating cached local store:
  resource temporarily unavailable` — an already-initialized store does not
  prevent it. Keyed cosign (`--key=…`) never contacts Fulcio/Rekor and keeps
  the full `--parallelism`.

      $ anodizer release --verbose
           Signing artifacts
           • keyless cosign: serializing 2 invocation(s) — concurrent invocations collide on the sigstore TUF trust store
           • signing 2 artifacts with parallelism=1

- **Transient retry** — failed cosign invocations are retried up to 5 attempts
  with jittered exponential backoff (2s base, 15s cap; ~29s total spread),
  since cosign depends on network infrastructure (Fulcio, Rekor, the TUF CDN).
  A missing cosign binary fails immediately. Local signers (gpg, osslsigncode,
  signtool) are deterministic and fail fast with no retry.
