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
`anodizer release` name the same binary's signature identically. The name is
unique per crate, target **and binary**:

| The binary's target | Asset name |
|---|---|
| Covered by the crate's primary `archives:` entry — the first entry in config order whose `ids:` / `binaries:` filters take this binary — and that entry packs this binary alone on the target | That entry's `name_template` rendered for the target, plus the suffix the `signature:` / `certificate:` template appended to the binary's file name. `app-1.2.3-linux-amd64.sig`, `app-1.2.3-windows-amd64.sig` (from `app.exe.sig`), `app-1.2.3-linux-amd64.bundle.sig` |
| Covered by an entry that packs several binaries on that target, or by no archive entry at all — a musl build that only feeds npm, say | `{{ Binary }}-{{ Version }}-{{ Target }}` plus the amd64 micro-architecture level and that suffix: `app-1.2.3-x86_64-unknown-linux-musl.sig`, `app-1.2.3-x86_64-unknown-linux-gnuv3.sig`. One archive carries the whole group under a single name, so each binary's signature takes the triple instead; the whole triple is required, because `Os`/`Arch` render identically for a gnu and a musl build; and the level is appended because a baseline and a `-Ctarget-cpu=x86-64-v3` build share one triple (`v1` renders nothing) |
| Covered by an entry whose resolved formats include `binary` — the entry's own `formats:`, or a `format_overrides:` entry matching the target's OS, or `defaults.archives.format_overrides` | That uploaded executable's own name plus the suffix, so a `signs:` config covering the asset and a `binary_signs:` config covering the same bytes resolve to one name |
| A lipo-merged universal binary (`darwin-universal`), which no `builds:` entry names | The covering entry's `name_template` rendered with the binary's own name |

An entry's `if:` is **not** evaluated when the primary is chosen. A gate can
read the environment, and a signature named one way on the machine that builds
it and another on the machine that publishes it is an asset the release's
verify gate cannot find.

A name that renders empty fails the sign stage rather than uploading a bare
`.sig` every binary's signature would collide on.

Two binaries resolving to ONE name fail it the same way. A release asset
carries one file, so the second upload would replace the first and the release
would ship a signature over bytes nobody can identify:

```text
Error: sign: the binaries 'target/x86_64-unknown-linux-gnu/release/app (crate 'app', build id 'app', target x86_64-unknown-linux-gnu, amd64 v1)' and 'target/x86_64-unknown-linux-gnu/release/app (crate 'app', build id 'app', target x86_64-unknown-linux-gnu, amd64 v3)' both resolve to the signature asset name 'app-1.2.3-linux-amd64.sig', rendered from the template '{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}'. One release asset cannot carry two signatures — give the covering `archives[].name_template` a variable that separates them ({{ Target }} and {{ Amd64 }} are the dimensions {{ Os }}-{{ Arch }} drops), or set `binary_signs[].asset_name_template`.
```

The certificate is checked the same way. Two `binary_signs:` entries whose
`signature:` templates differ name two distinct signatures over one base, but
if their `certificate:` templates match they render one certificate name — and
that fails the run too, naming the certificate asset.

When a `signature:` (or `certificate:`) template renders a name of its own
instead of suffixing the binary's file name, the base is not part of the
result, so the message asks for a change to that template rather than to the
archive name:

```text
Error: sign: the binaries '…, build id 'app', …' and '…, build id 'helper', …' both resolve to the signature asset name 'detached-x86_64-unknown-linux-gnu.sig', rendered by the `binary_signs[].signature:` template, which renamed the output instead of suffixing the binary's own file name — so the asset base 'app-1.2.3-linux-amd64' (from '{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}') is not part of it. One release asset cannot carry two signatures — give that template {{ .Artifact }} or the target, so it renders one name per binary.
```

The two dimensions a hand-written `{{ Os }}-{{ Arch }}` template drops are the
full target triple (a gnu and a musl build render one `linux-amd64`) and the
x86-64 micro-architecture level (a baseline and a `-Ctarget-cpu=x86-64-v3`
build render one `amd64`). Either dimension in the template separates them —
this is what the built-in default does:

```yaml
archives:
  - name_template: >-
      {{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}{% if Amd64 and Amd64 != "v1" %}{{ Amd64 }}{% endif %}
```

or name the signature directly:

```yaml
binary_signs:
  - asset_name_template: "{{ Binary }}-{{ Version }}-{{ Target }}"
```

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
  - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"   # id: default, single-variant
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

`asset_name_template:` replaces the derived name. It is read on `binary_signs:`
entries only — `anodizer check config` warns when a `signs:` entry sets it. It
renders the asset's BASE name in the same per-target scope an archive
`name_template` renders under
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
