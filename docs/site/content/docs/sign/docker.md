+++
title = "Docker Images"
description = "Sign Docker images with cosign"
weight = 2
template = "docs.html"
+++

Sign your Docker images after they're pushed.

## Config

```yaml
docker_signs:
  - artifacts: all
    cmd: cosign
    args: ["sign", "--key=cosign.key", "{{ Artifact }}@{{ Digest }}"]
```

## Docker sign config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | | Unique identifier for this docker sign config (referenced by `ids` filters elsewhere). |
| `artifacts` | string | `""` | Which Docker artifacts to sign: `all`, `images`, `manifests`, `none`, or `""` (empty — the default — signs the canonical Docker images). The singular `image` / `manifest` are **not** accepted and hard-error at release time. |
| `cmd` | string | `cosign` | Signing command to invoke. |
| `args` | list | `["sign", "--key=cosign.key", "{{ .Artifact }}@{{ .Digest }}", "--yes"]` | Arguments passed to the signing command. `{{ Artifact }}` is replaced by the digest-pinned image reference and `{{ Signature }}` by the synthesized `<image>@<digest>.sig` name before the rest is rendered as a template; `{{ Digest }}` renders the image digest. The `${artifact}` shell variables the [binary/archive path](@/docs/sign/binaries-archives.md) expands are **not** expanded here and reach the signing command as literal text. |
| `signature` | string | — | **Ignored.** A container signature is stored in the registry beside the image rather than written to a file, so anodizer synthesizes the `<image>@<digest>.sig` name its argv substitutes and reads this template nowhere. `anodizer check config` warns when it is set. |
| `certificate` | string | | Certificate file whose **presence** selects cosign's bundle verification mode. The path itself never reaches the signing command — `{{ Certificate }}` in `args:` renders empty. |
| `ids` | list | all | Only sign images from docker configs whose `id` is in this list. |
| `stdin` | string | | Content written to the signing command's stdin (e.g. a passphrase); rendered as a template (e.g. `{{ Env.GPG_PASSPHRASE }}`) with nothing substituted first, so neither `{{ Artifact }}` nor `${artifact}` names anything here. |
| `stdin_file` | string | | Path to a file whose content is written to the signing command's stdin. |
| `env` | list | | Environment variables passed to the signing command (`KEY=VALUE` strings). |
| `output` | bool | `false` | Capture and log the signing command's stdout/stderr. |
| `if` | string | | Template-conditional: skip this config when the rendered result is `false` or empty. An absent, empty or blank `if:` imposes no gate and always runs; the falsy test applies to what a non-blank gate renders. |

Images are signed one at a time. A keyless config (no `--key` argument) also
takes the same host-level advisory lock as keyless
[binary/archive signing](@/docs/sign/binaries-archives.md), so two anodizer processes on
one host queue on the sigstore TUF trust store instead of colliding on it.
