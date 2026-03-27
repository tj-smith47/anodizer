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

Anodize respects these environment variables:

| Variable | Description |
|----------|-------------|
| `GITHUB_TOKEN` | GitHub API token for releases and publishing |
| `CARGO_REGISTRY_TOKEN` | Token for crates.io publishing |
| `DOCKER_USERNAME` / `DOCKER_PASSWORD` | Docker registry credentials |

## Template access

All environment variables (both config-defined and inherited from the shell) are accessible in templates:

```yaml
name_template: "{{ ProjectName }}-{{ Env.BUILD_NUMBER }}"
```
