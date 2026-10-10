+++
title = "Environment Variables"
description = "Configure environment variables for template access and build customization"
weight = 3
template = "docs.html"
+++

## Config-defined variables

Define custom environment variables in your config's top-level `env` field:

```yaml
env:
  MY_VAR: "some_value"
  BUILD_TYPE: "release"
```

These are available in templates as `{{ Env.MY_VAR }}` and are set in the environment for all external commands (cargo, docker, nfpm, etc.).

## Per-target build environment

Set environment variables for specific build targets:

```yaml
crates:
  - name: myapp
    builds:
      - binary: myapp
        env:
          x86_64-unknown-linux-gnu:
            CC: "gcc"
            OPENSSL_DIR: "/usr/local/ssl"
          aarch64-unknown-linux-gnu:
            CC: "aarch64-linux-gnu-gcc"
```

## Standard environment variables

Anodizer respects these environment variables:

| Variable | Description |
|----------|-------------|
| `ANODIZER_GITHUB_TOKEN` | GitHub API token (takes precedence over `GITHUB_TOKEN`) |
| `GITHUB_TOKEN` | GitHub API token for releases and publishing |
| `CARGO_REGISTRY_TOKEN` | Token for crates.io publishing |
| `DOCKER_USERNAME` / `DOCKER_PASSWORD` | Docker registry credentials |

## GitHub release upload tuning

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `ANODIZER_GITHUB_UPLOAD_CONCURRENCY` | u32 | `4` | Cap on parallel asset uploads to a release (applies to every forge: GitHub, GitLab, Gitea; the `GITHUB` infix is historical). Override of `release.upload_concurrency:`. Keep low (≤8) to avoid GitHub's secondary rate limit when releases include many artifacts. |
| `ANODIZER_GITHUB_UPLOAD_PACE_MS` | integer milliseconds | `200` | Proactive minimum interval between successive asset-upload *starts* on any forge, jittered ±20% and layered on top of the concurrency cap and the reactive backoff. Override of `release.upload_pace:`. Smooths the initial burst that trips GitHub's secondary rate limit. Set to `0` to disable pacing (rely on the cap + backoff). |
| `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS` | integer seconds | `60` | Sleep duration after a GitHub secondary rate-limit response (403/429 carrying the `"secondary rate limit"` marker in the body or `secondary-rate-limits` in the `documentation_url`). Applied with ±20% jitter before the next upload retry. |

## Secret redaction in log output

Anodizer masks secret values in everything it prints: status lines, the
`-v` command echoes, a subprocess's streamed output and error text. A masked
value is replaced with `$` and the name of the variable that holds it. The
variables it reads are the process environment, the config's `env:` block,
and the `env:` of the job whose command is running (a `signs:`, `dockers:`,
`sboms:`, `publishers:` or build entry).

A variable is treated as a secret when its value is not empty and either
rule holds:

| Rule | Matches |
|------|---------|
| The name, compared without case, ends in `_KEY`, `_SECRET`, `_PASSWORD`, `_TOKEN` or `_PASSPHRASE`, or contains `CREDENTIAL` or `APIKEY`, and the value is not one of `true`, `false`, `yes`, `no`, `on`, `off`, `0`, `1` | `GITHUB_TOKEN`, `COSIGN_PASSWORD`, `GOOGLE_CREDENTIALS_JSON` |
| The value starts with a known token prefix, whatever the name: `sk-`, `ghp_`, `ghs_`, `gho_`, `ghu_`, `github_pat_`, `dckr_pat_`, `glpat-`, `AIza`, `ya29.`, `xox` | `DEPLOY=ghp_…` |

Where a secret value is masked depends on its length:

| Value length | Masked |
|--------------|--------|
| 8 characters or more | every occurrence, including inside a longer word (`-p<value>`) |
| under 8 characters | only where it stands alone, not inside a longer word |

Credentials written inside a URL (`https://user:pass@host`) are replaced
with `<redacted>` whatever variable they came from.

Two consequences follow from matching on the name:

```yaml
signs:
  - env:
      - COSIGN_KEY=cosign.key        # name ends in _KEY: the path is masked
      - REGISTRY_AUTH=hunter2hunter2 # plain name, no known prefix: NOT masked
```

| Argument the command was given | Printed as |
|--------------------------------|------------|
| `--key=cosign.key` | `--key=$COSIGN_KEY` |
| `--password=hunter2hunter2` | `--password=hunter2hunter2` |

A file path or any other harmless value stored under a secret-shaped name
prints as `$NAME`, and a real secret stored under a name that matches no
rule is printed as it is. Name the variable that holds a secret with one of
the suffixes above.

## Template access

All environment variables (both config-defined and inherited from the shell) are accessible in templates:

```yaml
name_template: "{{ ProjectName }}-{{ Env.BUILD_NUMBER }}"
```
