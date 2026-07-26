+++
title = "Config Includes"
description = "Split configuration across multiple files"
weight = 3
template = "docs.html"
+++

Anodizer lets you split your configuration across multiple files using the
`includes` field. Included files provide **defaults** that the base config
can selectively override, making it easy to share common settings across
projects or separate concerns into focused files.

## Basic usage

Add an `includes` list to your `.anodizer.yaml`. Each entry points to another
YAML or TOML config file:

```yaml
# .anodizer.yaml
project_name: my-app
includes:
  - configs/defaults.yaml
  - configs/packaging.yaml
crates:
  - name: my-app
    path: .
```

Paths are **relative to the config file** that contains the `includes` field.
Absolute paths are rejected.

## Include forms

There are three ways to specify an include entry.

### Plain string (file path)

The simplest form -- a relative path to a local file:

```yaml
includes:
  - ./defaults.yaml
  - configs/nfpm.yaml
```

### Structured file (`from_file`)

An explicit file include with a `path` key. Functionally identical to a plain
string but can be clearer when mixed with URL includes:

```yaml
includes:
  - from_file:
      path: ./configs/shared.yaml
```

### URL (`from_url`)

Fetch a config file from a URL at load time. Useful for sharing config across
repositories:

```yaml
includes:
  - from_url:
      url: https://example.com/shared/base-config.yaml
```

#### GitHub shorthand

URLs that do not start with `http://` or `https://` are treated as GitHub
raw-content paths. Anodizer prepends `https://raw.githubusercontent.com/`
automatically:

```yaml
includes:
  # Expands to: https://raw.githubusercontent.com/myorg/shared-configs/main/anodizer.yaml
  - from_url:
      url: myorg/shared-configs/main/anodizer.yaml
```

#### Custom headers

Add HTTP headers for authentication or other needs. Header values support
`${VAR_NAME}` environment variable expansion:

```yaml
includes:
  - from_url:
      url: https://raw.githubusercontent.com/myorg/private-configs/main/base.yaml
      headers:
        Authorization: "Bearer ${GITHUB_TOKEN}"
        x-custom-header: "some-value"
```

The URL itself also supports `${VAR_NAME}` expansion:

```yaml
includes:
  - from_url:
      url: https://${CONFIG_HOST}/configs/base.yaml
```

URL includes auto-detect format by file extension: `.toml` files are parsed as
TOML and converted internally; everything else is parsed as YAML.

There is a 10 MB size limit on URL responses.

## Mixing include forms

All three forms can be used together in the same `includes` list:

```yaml
includes:
  - ./defaults.yaml
  - from_file:
      path: ./configs/packaging.yaml
  - from_url:
      url: myorg/shared-configs/main/common.yaml
      headers:
        Authorization: "Bearer ${GITHUB_TOKEN}"
```

## Merge strategy

Included files are **defaults** -- the base config always wins.

The merge algorithm works as follows:

1. **Included files are merged together in order.** The first include forms
   the initial layer; each subsequent include is deep-merged on top of it.
2. **The base config is merged on top of the accumulated includes.** Any value
   set in the base config overrides the same value from includes.

The deep-merge rules:

| Type | Behavior |
|------|----------|
| **Objects/mappings** | Merged recursively -- keys from both sides are kept, with the overlay's value winning on conflict |
| **Arrays/sequences** | Concatenated -- the overlay's items are appended after the base's items |
| **Scalars** | The overlay value replaces the base value |

### Example

Given these files:

```yaml
# defaults.yaml (included file)
dist: /default/dist
env:
  DEPLOY_ENV: staging
crates:
  - name: shared-lib
    path: crates/shared
```

```yaml
# .anodizer.yaml (base config)
project_name: my-app
includes:
  - defaults.yaml
dist: /my/custom/dist
crates:
  - name: my-app
    path: .
```

The effective config is:

```yaml
project_name: my-app
dist: /my/custom/dist          # base wins over include
env:
  DEPLOY_ENV: staging           # from include (not overridden by base)
crates:
  - name: shared-lib            # from include (arrays concatenate)
    path: crates/shared
  - name: my-app                # from base
    path: .
```

## TOML configs

Includes work in TOML config files too. Use TOML array-of-tables syntax for
structured includes:

```toml
# .anodizer.toml
project_name = "my-app"
includes = ["defaults.yaml"]
crates = []
```

For structured forms:

```toml
[[includes]]
[includes.from_file]
path = "configs/shared.yaml"

[[includes]]
[includes.from_url]
url = "myorg/shared-configs/main/common.yaml"
```

Included files can be either YAML or TOML regardless of the base config's
format. The format is detected by file extension.

## Use cases

### Shared org-wide defaults

Host a base config in a central repository and include it in every project:

```yaml
# In each project's .anodizer.yaml
includes:
  - from_url:
      url: myorg/release-configs/main/base.yaml
      headers:
        Authorization: "Bearer ${GITHUB_TOKEN}"

project_name: my-service
crates:
  - name: my-service
    path: .
```

The shared config can define standard settings for changelog format, checksum
algorithms, signing, announcement webhooks, and more. Each project overrides
only what it needs.

### Splitting a large config

Break a complex config into focused files:

```yaml
# .anodizer.yaml
project_name: my-app
includes:
  - configs/builds.yaml        # build matrix, cross-compilation
  - configs/packaging.yaml     # nFPM, Snapcraft, archive formats
  - configs/publishing.yaml    # Homebrew, Scoop, AUR, Krew
  - configs/announce.yaml      # Slack, Discord, webhooks
crates:
  - name: my-app
    path: .
```

### Per-environment overrides

Use a base config for shared settings and environment-specific includes:

```yaml
# configs/base.yaml
changelog:
  sort: asc
  filters:
    exclude:
      - "^docs(\\(.*\\))?!?:"
      - "^test(\\(.*\\))?!?:"
checksum:
  algorithm: sha256
```

```yaml
# configs/staging.yaml
release:
  draft: true
  prerelease: auto
```

```yaml
# .anodizer.yaml
project_name: my-app
includes:
  - configs/base.yaml
  - configs/staging.yaml
crates:
  - name: my-app
    path: .
```

### Reusable variables with includes

The `variables` field pairs well with includes. Define variables in a shared
config and reference them in project-specific settings:

```yaml
# shared/variables.yaml
variables:
  org_name: myorg
  registry: ghcr.io/myorg
```

```yaml
# .anodizer.yaml
includes:
  - shared/variables.yaml
project_name: my-app
crates:
  - name: my-app
    path: .
```
