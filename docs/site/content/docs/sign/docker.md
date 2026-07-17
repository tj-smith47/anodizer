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
    args: ["sign", "--key=cosign.key", "${artifact}"]
```

## Docker sign config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | | Unique identifier for this docker sign config (referenced by `ids` filters elsewhere). |
| `artifacts` | string | `""` | Which Docker artifacts to sign: `all`, `images`, `manifests`, `none`, or `""` (empty — the default — signs the canonical Docker images). The singular `image` / `manifest` are **not** accepted and hard-error at release time. |
| `cmd` | string | `cosign` | Signing command to invoke. |
| `args` | list | — | Arguments passed to the signing command. Templates supported. |
| `signature` | string | auto | Signature output filename template. Templates supported. |
| `certificate` | string | | Certificate file to embed in the signature (Cosign bundle signing). |
| `ids` | list | all | Only sign images from docker configs whose `id` is in this list. |
| `stdin` | string | | Content written to the signing command's stdin (e.g. a passphrase); template-expanded (e.g. `{{ Env.GPG_PASSPHRASE }}`). |
| `stdin_file` | string | | Path to a file whose content is written to the signing command's stdin. |
| `env` | list | | Environment variables passed to the signing command (`KEY=VALUE` strings). |
| `output` | bool | `false` | Capture and log the signing command's stdout/stderr. |
| `if` | string | | Template-conditional: skip this config when the rendered result is `false` or empty. |
