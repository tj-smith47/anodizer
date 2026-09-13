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
| `if` | string | none | Template-conditional: skip this config when the rendered result is `false` or empty. An absent, empty or blank `if:` imposes no gate and always runs; the falsy test applies to what a non-blank gate renders. |

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
       Error sign: the signature of 'target/x86_64-unknown-linux-gnu/release/app (crate 'app', build id 'app', target x86_64-unknown-linux-gnu, amd64 v1)' and the signature of 'target/x86_64-unknown-linux-gnu/release/app (crate 'app', build id 'app', target x86_64-unknown-linux-gnu, amd64 v3)' both resolve to the asset name 'app-1.2.3-linux-amd64.sig' — the signature is the base 'app-1.2.3-linux-amd64' rendered from the template '{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}' plus the suffix `binary_signs[].signature:` appended. One release asset cannot carry both files — give the covering `archives[].name_template` a variable that separates them ({{ Target }} and {{ Amd64 }} are the dimensions {{ Os }}-{{ Arch }} drops), or set `binary_signs[].asset_name_template`.
```

The certificate is checked the same way. Two `binary_signs:` entries whose
`signature:` templates differ name two distinct signatures over one base, but
if their `certificate:` templates match they render one certificate name — and
that fails the run too, naming the certificate asset.

One entry's own two outputs are checked against each other as well: a
`signature:` and a `certificate:` template that append the same suffix render
one name for two files, and the run stops with both outputs named:

```text
       Error sign: the signature and the certificate of 'target/x86_64-unknown-linux-gnu/release/app (crate 'app', build id 'app', target x86_64-unknown-linux-gnu, amd64 v1)' both resolve to the asset name 'app-1.2.3-linux-amd64.sig' — the signature is the base 'app-1.2.3-linux-amd64' rendered from the template '{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}' plus the suffix `binary_signs[].signature:` appended and the certificate is the base 'app-1.2.3-linux-amd64' rendered from the template '{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}' plus the suffix `binary_signs[].certificate:` appended. One release asset cannot carry both files — give `binary_signs[].signature:` and `binary_signs[].certificate:` names that differ.
```

When a `signature:` (or `certificate:`) template renders a name of its own
instead of suffixing the binary's file name, the base is not part of the
result, so the message asks for a change to that template rather than to the
archive name:

```text
       Error sign: the signature of 'target/x86_64-unknown-linux-gnu/release/app (crate 'app', build id 'app', target x86_64-unknown-linux-gnu, amd64 v1)' and the signature of 'target/x86_64-unknown-linux-gnu/release/helper (crate 'app', build id 'helper', target x86_64-unknown-linux-gnu, amd64 v1)' both resolve to the asset name 'detached-x86_64-unknown-linux-gnu.sig' — the signature was rendered by the `binary_signs[].signature:` template, which renamed the output instead of suffixing the binary's own file name, so the asset base 'app-1.2.3-linux-amd64' (from '{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}') is not part of it. One release asset cannot carry both files — give the `binary_signs[].signature:` template {{ .Artifact }} or the target, so it renders one name per binary.
```

`asset_name_template:` is a per-entry field and the asset name is the base
plus the suffix, so two entries can trade a component between the two —
`app-{{ Version }}.bundle` with `{{ .Artifact }}.sig`, and `app-{{ Version }}`
with `{{ .Artifact }}.bundle.sig` — and resolve one binary's signature to one
asset name over two files. That fails the run too, naming both files:

```text
       Error sign: two `binary_signs:` entries resolve the signature of 'target/x86_64-unknown-linux-gnu/release/app (crate 'app', build id 'app', target x86_64-unknown-linux-gnu, amd64 v1)' to one asset name 'app-1.2.3.bundle.sig' over two files ('dist/target/x86_64-unknown-linux-gnu/release/app.sig' and 'dist/target/x86_64-unknown-linux-gnu/release/app.bundle.sig'). One release asset carries one file — give the two entries `signature:` suffixes that differ, or one of them its own `asset_name_template`.
```

Two entries that resolve to one name over ONE file are accepted — that is one
release asset — but the second `cmd:` overwrites the first's bytes, so one
signature ships where two were configured:

```yaml
project_name: app
binary_signs:
  - cmd: cosign
    args: ["sign-blob", "--key=cosign.key", "--output-signature=${signature}", "${artifact}"]
  - cmd: gpg
    args: ["--batch", "--detach-sig", "--output", "${signature}", "${artifact}"]
```

`anodizer check config` says so:

```text
   • validating configuration
     Warning binary_signs[0] and binary_signs[1] resolve one signature file for the artifacts both select — the second signature overwrites the first, so one file ships where two were configured
   • Config is valid.
```

The two dimensions a hand-written `{{ Os }}-{{ Arch }}` template drops are the
full target triple (a gnu and a musl build render one `linux-amd64`) and the
x86-64 micro-architecture level (a baseline and a `-Ctarget-cpu=x86-64-v3`
build render one `amd64`). Either dimension in the template separates them —
this is what the built-in default does:

```yaml
crates:
  - name: app
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
crates:
  - name: app
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

Those two select different artifact kinds, so neither can overwrite the
other's output and `anodizer check config` says nothing about the pair.

## What `anodizer check config` catches

### Two entries writing one file

The duplicate-output question is asked of `signs:`, of `binary_signs:` and of
every per-crate slice, because all four resolve their outputs the same way and
write under the same `dist`. Two entries meet when their `artifacts:`, `if:`
and `ids:` selectors all can take one artifact:

```yaml
signs:
  - id: gpg
    artifacts: all
    cmd: gpg
    signature: "${artifact}.asc"
  - id: cosign
    artifacts: all
    cmd: cosign
    signature: "${artifact}.asc"
```

```text
   • validating configuration
     Warning signs[0] and signs[1] resolve one signature file for the artifacts both select — the second signature overwrites the first, so one file ships where two were configured
   • Config is valid.
```

A per-crate slice names the workspace it belongs to, and a filter that
`binary_signs:` cannot honor is named where it was written — under
`defaults.binary_signs:`, which is the one block that reaches the slice
without passing the `binary_signs:` loader:

```yaml
project_name: app
workspaces:
  - name: tools
    crates:
      - name: app
    binary_signs:
      - cmd: cosign
      - cmd: gpg
defaults:
  binary_signs:
    artifacts: archive
```

```text
   • validating configuration
     Warning defaults.binary_signs artifacts filter 'archive' is not allowed on binary_signs (valid: binary, none) — the sign stage signs binaries whatever it says
     Warning workspaces.tools.binary_signs[0] and workspaces.tools.binary_signs[1] resolve one signature file for the artifacts both select — the second signature overwrites the first, so one file ships where two were configured
   • Config is valid.
```

### A placeholder the field cannot substitute

`{{ .Artifact }}`, `{{ .Signature }}` and `{{ .Certificate }}` are replaced by
exact literal before the template reaches the engine, and which of the three a
field replaces differs per field: `args:` takes all three, `signature:` and
`certificate:` take `Artifact` alone, and `stdin:` takes none. Every other
spelling — a different padding, or the name inside an expression — survives the
replacement, reaches the engine as an undefined variable and fails the sign
stage. A `docker_signs:` entry is narrower still: its argv substitutes the same
three names, but the `${…}` variables below are never expanded there, and its
`signature:` is read nowhere at all, because a container signature is stored in
the registry:

```yaml
project_name: app
binary_signs:
  - cmd: cosign
    signature: "{{.Artifact}}.sig"
    certificate: "{{ Artifact | upper }}.pem"
  - cmd: gpg
    signature: "{{ .Signature }}.asc"
docker_signs:
  - cmd: cosign
    args: ["sign", "--key=cosign.key", "${artifact}"]
    signature: "{{ .Artifact }}.sig"
```

```text
   • validating configuration
     Warning binary_signs[0].signature names `{{.Artifact}}`, which anodizer substitutes only as the literal `{{ .Artifact }}` or `{{ Artifact }}` — every other spelling reaches the template engine as an undefined variable and fails the sign stage
     Warning binary_signs[0].certificate names `{{ Artifact | upper }}`, which anodizer substitutes only as the literal `{{ .Artifact }}` or `{{ Artifact }}` — every other spelling reaches the template engine as an undefined variable and fails the sign stage
     Warning binary_signs[1].signature names `{{ .Signature }}`, which anodizer does not substitute in signature: — it reaches the template engine as an undefined variable and fails the sign stage; the signature path is what this template renders, so `${signature}` has no value here either — remove the reference
     Warning docker_signs[0].args names `${artifact}`, which the docker sign path never expands — it reaches the signing command as that literal text; write `{{ .Artifact }}`, which anodizer substitutes before the render
     Warning docker_signs[0].signature is set but a docker signature is stored in the registry rather than written to a file (it will be ignored)
   • Config is valid.
```

The `${…}` variables are the way to name another output on the binary and
archive path: `${certificate}` inside `signature:` and `${signature}` inside
`certificate:` are expanded after the render. A field's own name is not —
`${signature}` inside `signature:` expands to that template's own unexpanded
text — so there the reference has to go. A `docker_signs:` entry expands none
of them.

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
