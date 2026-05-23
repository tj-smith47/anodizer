+++
title = "NPM"
description = "Publish NPM binary wrapper packages"
weight = 86
template = "docs.html"
+++

Anodizer can publish NPM packages that wrap your compiled binaries, allowing users to install them via `npm install -g`.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | false | warn-only (npm unpublish has a 72-hour window; after that, the version is permanent) | NPM auth token (via `.npmrc` or `npm login`) |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## Minimal config

```yaml
npms:
  - name: "@myorg/myapp"
```

## NPM config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | none | Unique identifier |
| `name` | string | **required** | NPM package name (e.g., `@myorg/myapp`) |
| `description` | string | none | Package description |
| `homepage` | string | none | Homepage URL |
| `keywords` | list | none | NPM package keywords |
| `license` | string | none | License identifier |
| `author` | string | none | Package author |
| `repository` | string | none | Git repository URL |
| `bugs` | string | none | Bug tracker URL |
| `access` | string | none | NPM access level (`"public"` or `"restricted"`) |
| `tag` | string | `latest` | NPM dist-tag |
| `format` | string | `tgz` | Download archive format (`tgz` or `zip`) |
| `ids` | list | none | Filter by build IDs |
| `url_template` | string | auto-derived | Download URL for binaries (template) |
| `extra_files` | list | none | Additional files to include |
| `templated_extra_files` | list | none | Template-rendered extra files |
| `extra` | map | none | Extra fields merged into package.json root |
| `if` | string | none | Template condition; skip if result is `"false"` or empty |
| `disable` | string/bool | none | Disable this config |

## How it works

Anodizer generates:

1. A `package.json` with a `postinstall` script
2. A `postinstall.js` that detects the user's OS and architecture, downloads the correct binary from your release, and installs it

Users install with `npm install -g @myorg/myapp` and get a working binary.

## Full config reference

```yaml
npms:
  - name: "@myorg/myapp"       # required
    id: ""                     # unique identifier
    description: ""
    homepage: ""
    keywords: []
    license: ""
    author: ""
    repository: ""             # git URL
    bugs: ""                   # bug tracker URL
    access: public             # public | restricted
    tag: latest                # npm dist-tag
    format: tgz                # tgz | zip
    ids: []                    # filter by build IDs
    url_template: ""           # download URL override
    extra_files: []
    templated_extra_files: []
    extra: {}                  # extra fields merged into package.json root
    if: ""                     # template condition; skip if result is "false"
    disable: false
```

## Authentication

NPM authentication uses the standard `npm` CLI auth mechanism. Configure credentials via `.npmrc` or `npm login` before running the release.

## Common gotchas

- **Authentication required**: npm must be authenticated before the release runs. Use `npm login` locally or configure a token in `.npmrc` (e.g. `//registry.npmjs.org/:_authToken=${NPM_TOKEN}`).
- **`access: public`** is required for scoped packages (`@myorg/myapp`) on the public npm registry. Without it, scoped packages default to `restricted` and the publish fails with an access error.
- **72-hour unpublish window**: npm allows `npm unpublish` within 72 hours of a version being published. After that, the version is permanent on the registry.

## Conditional publishing

Use `if` to conditionally skip NPM publishing:

```yaml
npms:
  - name: "@myorg/myapp"
    if: "{{ ne .Prerelease \"\" }}"
```

## Full example

```yaml
npms:
  - name: "@myorg/myapp"
    description: "A fast CLI tool"
    homepage: "https://example.com/myapp"
    license: MIT
    author: "My Org"
    repository: "https://github.com/myorg/myapp"
    bugs: "https://github.com/myorg/myapp/issues"
    access: public
    keywords:
      - cli
      - tool
    extra:
      engines:
        node: ">=14"
```
