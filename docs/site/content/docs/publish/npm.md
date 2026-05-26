+++
title = "NPM"
description = "Publish NPM binary wrapper packages"
weight = 86
template = "docs.html"
+++

Anodizer publishes NPM packages that wrap your compiled binaries, letting users install your CLI via `npm install -g <name>`. The published package carries a `postinstall.js` shim that downloads the matching OS/arch archive from your release and extracts the binary into `bin/`.

This publisher targets the same use-case as biome, swc, and rolldown: distributing a Rust CLI through the npm registry without forcing every consumer to have a Rust toolchain.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | true | `npm unpublish` (within 72 hours) | `NPM_TOKEN` (`.npmrc` `_authToken=...`) |

A failed `npm publish` aborts the release by default — npm is load-bearing for consumers running `npm install`. Set `required: false` to log + continue on failure.

## Minimal config

```yaml
npms:
  - name: "@anodize/demo"
    access: public        # required for new scoped packages
```

Run with `NPM_TOKEN=<your token>` exported and anodizer will:

1. Collect the release archive artifacts (one per OS/arch).
2. Render a `package.json` whose `anodize.binaries` table maps Node's `process.platform`/`process.arch` to the per-platform download URL + sha256.
3. Render a `postinstall.js` shim that selects the matching entry, downloads it, sha256-verifies, and extracts the binary into `node_modules/<name>/bin/`.
4. Pack a deterministic `.tgz` and run `npm publish --tag <tag> --registry <registry>`.

## NPM config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | none | Unique identifier (for `--id=...` selection) |
| `ids` | list | none | Filter archives by build ID (matches `crate_name`) |
| `name` | string | crate name | NPM package name (e.g. `@anodize/demo`) |
| `description` | string | `metadata.description` | Package description |
| `homepage` | string | `metadata.homepage` | Homepage URL |
| `keywords` | list | none | NPM package keywords |
| `license` | string | `metadata.license` | License identifier |
| `author` | string | none | Package author |
| `repository` | string | none | Git repository URL |
| `bugs` | string | none | Bug tracker URL |
| `access` | string | none | NPM access level (`public` or `restricted`) |
| `tag` | string | `latest` | NPM dist-tag |
| `format` | string | `tgz` | Download archive format (`tgz`, `tar.gz`, `zip`, `binary`) |
| `url_template` | string | derived | Download URL template override |
| `registry` | string | `https://registry.npmjs.org` | Registry endpoint |
| `token` | string | `NPM_TOKEN` env var | Auth token (templated; prefer env var) |
| `extra_files` | list | `[README*, LICENSE*]` | Glob set of files to include in the tarball |
| `templated_extra_files` | list | none | Template-rendered file mappings (`{src, dst}`) |
| `extra` | map | none | Free-form root-level `package.json` fields (shallow-merged) |
| `skip` | string/bool | none | Skip this publisher (template-conditional) |
| `disable` | string/bool | none | Disable this publisher entry |
| `if` | string | none | Template condition; skip if result is falsy |
| `required` | bool | `true` | Whether failure here aborts the release |

## Authentication

Set the `NPM_TOKEN` env var to your npm auth token; anodizer writes a process-private `.npmrc` carrying `//registry.npmjs.org/:_authToken=$NPM_TOKEN` and passes `--userconfig <that .npmrc>` to `npm publish`. The token is never placed on the argv and the `.npmrc` is deleted after publish completes.

For a private registry (e.g. GitHub Packages):

```yaml
npms:
  - name: "@anodize/demo"
    registry: "https://npm.pkg.github.com"
    access: restricted
```

## How it works

Anodizer generates the following layout inside the published tarball:

```
package/
├── package.json
├── postinstall.js
├── bin/
│   └── <name>.js          # launcher (spawns the native binary)
├── README.md              # from extra_files
└── LICENSE                # from extra_files
```

Users install with `npm i -g @anodize/demo`:

1. npm extracts `package/` into `node_modules/@anodize/demo/`.
2. npm runs `postinstall.js`, which:
   - Reads `package.json::anodize.binaries`.
   - Picks the entry whose `os`/`cpu` matches `process.platform`/`process.arch`.
   - Downloads the archive over HTTPS (with redirect following).
   - Verifies the sha256.
   - Extracts the binary into `bin/<name>` (or `bin/<name>.exe` on Windows).
3. npm symlinks `bin/<name>.js` into `node_modules/.bin/<name>`, which spawns the native binary.

## Scoped vs unscoped packages

Scoped packages (`@org/name`) on npmjs.org default to **restricted** access unless `access: public` is set. Without `access: public`, the first publish of a scoped package on a free npm account will fail with a clear error.

## Custom registries

Anodizer supports any npm-compatible registry — set `registry:` to the endpoint URL. Common examples:

```yaml
# GitHub Packages
npms:
  - name: "@myorg/cli"
    registry: "https://npm.pkg.github.com"
    access: restricted

# Verdaccio (self-hosted)
npms:
  - name: "myorg-cli"
    registry: "https://npm.myorg.example.com"
```

## Rollback

Within the 72-hour window after publishing, `anodize publish --rollback-only` runs `npm unpublish <name>@<version> --force` for each recorded target. Outside the window, npm refuses unpublish requests, and anodizer surfaces a warning pointing at `npm deprecate` as the remaining remediation surface.

## Common gotchas

- **`NPM_TOKEN` required for non-dry-run publishes.** Anodizer hard-errors when the token is unset and `--dry-run` is not active. The error message names the env var.
- **`access: public` required for scoped packages.** Without it, the first publish to a free npm account fails with `403`.
- **`--ignore-scripts` skips the postinstall.** End-users with `npm install --ignore-scripts` will see an installed package whose binary is missing. There is no workaround on the publisher side; document this in your install instructions.
- **72-hour unpublish window.** After that, the version is permanent. The publisher's `required: true` default exists to give you a chance to spot a bad release before that window closes.

## Conditional publishing

Use `if` to gate publishing on a templated condition:

```yaml
npms:
  - name: "@anodize/demo"
    if: "{{ ne .Prerelease \"\" }}"  # only on prereleases
```

## Full example

```yaml
npms:
  - id: primary
    name: "@anodize/demo"
    description: "A fast Rust CLI shipped via npm"
    homepage: "https://example.com/demo"
    license: MIT
    author: "Anodize Team"
    repository: "https://github.com/anodize/demo"
    bugs: "https://github.com/anodize/demo/issues"
    access: public
    tag: latest
    keywords:
      - cli
      - rust
    extra:
      engines:
        node: ">=14"
```
