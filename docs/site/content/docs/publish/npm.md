+++
title = "NPM"
description = "Publish prebuilt binaries through the npm registry"
weight = 86
template = "docs.html"
+++

Anodizer publishes your compiled binaries through the npm registry, letting users install your CLI via `npm install -g <name>`. This is how leading Rust CLIs ship binaries through npm — biome treats npm as its *primary* distribution channel, and git-cliff ships the same way.

Two distribution modes are supported, selected by `mode:`:

| `mode` | What it emits | When to use |
|--------|---------------|-------------|
| `optional-deps` (**default**) | One thin per-platform package per built target + a metapackage whose `optionalDependencies` list them. npm's native `os`/`cpu`/`libc` resolution installs only the matching prebuilt package. No download, no postinstall. | The modern default. The biome / git-cliff pattern. |
| `postinstall` | A single package carrying a `postinstall.js` shim that downloads + sha256-verifies the matching release archive at install time. | Registries or policies that disallow per-platform packages. |

## Classification

| Group | Required (default) | Rollback | Token scope |
|-------|--------------------|----------|-------------|
| Manager | `true` | `npm unpublish` (72h window) | `NPM_TOKEN` |

## Quick start (optional-deps)

```yaml
npms:
  - scope: "@biomejs"      # scope for the per-platform packages
    metapackage: biome     # the package users `npm install`
    bin: biome             # command name the metapackage installs
    access: public         # required for new scoped packages
```

Run with `NPM_TOKEN=<your token>` exported. For each built target anodizer:

1. Derives the npm `os`/`cpu`/`libc` selectors from the target triple (e.g. `x86_64-unknown-linux-musl` → `{ os: [linux], cpu: [x64], libc: [musl] }`).
2. Emits one per-platform package `@scope/<bin>-<os>-<cpu>[-<libc>]` embedding the prebuilt binary (mode `0o755`).
3. Emits the metapackage, listing every per-platform package under `optionalDependencies` and shipping a `bin` shim that resolves the installed one via `require.resolve` and execs it.
4. Publishes the per-platform packages first, then the metapackage, so the optional dependencies resolve at install time.

The `os`/`cpu`/`libc` triples are **always derived from the actual built targets** — never hand-written — so `npm install` resolves the right package on every consumer's platform. npm's naming differs from anodizer's internal naming (npm `os`: `linux`/`darwin`/`win32`; npm `cpu`: `x64`/`arm64`/`ia32`; npm `libc`: `musl`/`glibc`), and anodizer maps between them automatically (including `gnu` → npm's `glibc`).

### Generated layout (optional-deps)

```
@scope/cli-linux-x64-musl    package.json: { os:[linux], cpu:[x64], libc:[musl] }  + binary (0o755)
@scope/cli-linux-x64-glibc   package.json: { os:[linux], cpu:[x64], libc:[glibc] } + binary (0o755)
@scope/cli-darwin-arm64      package.json: { os:[darwin], cpu:[arm64] }            + binary (0o755)
@scope/cli-win32-x64         package.json: { os:[win32],  cpu:[x64] }              + binary (0o755)
cli  (metapackage)           package.json: { optionalDependencies: { …all above… }, bin: { cli: shim.js } }
                             shim.js: require.resolve(<matching pkg>) + spawnSync(...)  (musl detection, BINARY_OVERRIDE)
```

### libc-aware linux packages

By default (`libc_aware: true`) anodizer emits **separate** packages for linux musl and glibc, distinguished by the npm `libc` selector — musl and glibc binaries are not interchangeable, so collapsing them risks installing the wrong one. Set `libc_aware: false` to emit a single linux package per cpu with no `libc` selector (matching tools that ship a single linux binary):

```yaml
npms:
  - scope: "@acme"
    metapackage: cli
    libc_aware: false     # one @acme/cli-linux-x64 instead of -musl / -glibc
```

## postinstall mode

```yaml
npms:
  - mode: postinstall
    name: "@anodize/demo"
    access: public
    format: tgz           # archive format the shim downloads
```

In this mode anodizer collects the release **archive** artifacts, renders a `package.json` whose `anodize.binaries` table maps `process.platform`/`process.arch` to the per-platform download URL + sha256, and a `postinstall.js` shim that selects the matching entry, downloads it, sha256-verifies, and extracts the binary into `bin/`.

```
package/
├── package.json
├── postinstall.js
├── bin/
│   └── <name>.js          # launcher (spawns the native binary)
├── README.md              # from extra_files
└── LICENSE                # from extra_files
```

> **`--ignore-scripts` skips the postinstall.** End-users running `npm install --ignore-scripts` get an installed package whose binary is missing. The `optional-deps` mode does not have this failure mode (it has no install scripts) — another reason it is the default.

## NPM config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `mode` | string | `optional-deps` | Distribution strategy: `optional-deps` or `postinstall` |
| `scope` | string | none | npm scope for per-platform packages (`optional-deps`; required) |
| `metapackage` | string | `name`/crate name | Metapackage name users install (`optional-deps`) |
| `bin` | string | metapackage basename | Command name the metapackage installs (`optional-deps`) |
| `libc_aware` | bool | `true` | Emit linux musl/glibc as separate packages (`optional-deps`) |
| `id` | string | none | Unique identifier (for `--id=...` selection) |
| `ids` | list | none | Filter artifacts by build ID (matches `crate_name`) |
| `name` | string | crate name | Package name (postinstall package, or metapackage fallback) |
| `description` | string | `metadata.description` | Package description |
| `homepage` | string | `metadata.homepage` | Homepage URL |
| `keywords` | list | none | NPM package keywords |
| `license` | string | `metadata.license` | License identifier |
| `author` | string | none | Package author |
| `repository` | string | none | Git repository URL |
| `bugs` | string | none | Bug tracker URL |
| `access` | string | none | NPM access level (`public` or `restricted`) |
| `tag` | string | `latest` | NPM dist-tag |
| `format` | string | `tgz` | Download archive format (`postinstall` only) |
| `url_template` | string | derived | Download URL template override (`postinstall` only) |
| `registry` | string | `https://registry.npmjs.org` | Registry endpoint |
| `token` | string | `NPM_TOKEN` env var | Auth token (templated; prefer env var). Optional under Trusted Publishing — omit it to authenticate via GitHub Actions OIDC. |
| `extra_files` | list | `[README*, LICENSE*]` | Glob set of files to include |
| `templated_extra_files` | list | none | Template-rendered file mappings (`{src, dst}`) |
| `extra` | map | none | Free-form root-level `package.json` fields (shallow-merged) |
| `skip` | string/bool | none | Skip this publisher (template-conditional; legacy `disable:` spelling accepted as an alias) |
| `if` | string | none | Template condition; skip if result is falsy |
| `required` | bool | `true` | Whether failure here aborts the release |

## Authentication

Anodizer authenticates to npm in one of two ways, resolved per publish. It never publishes anonymously: with neither credential present it hard-errors.

### Trusted Publishing (tokenless OIDC) — recommended

Under GitHub Actions, npm's [Trusted Publishing](https://docs.npmjs.com/trusted-publishers) exchanges the workflow's OIDC token for a short-lived publish credential — **no long-lived `NPM_TOKEN` secret in the workflow**, and [provenance](https://docs.npmjs.com/generating-provenance-statements) is attached automatically.

Requirements:

- **npm CLI ≥ 11.5.1** and **Node ≥ 22.14.0** on the runner (the version that ships the OIDC exchange).
- The publishing job grants `permissions: id-token: write`.
- A Trusted Publisher is configured on npmjs.com for the package.

Configure the Trusted Publisher once per package: **npmjs.com → the package → Settings → Trusted Publishing → Add a GitHub Actions publisher**, with:

| Field | Value |
|-------|-------|
| Organization or user | `tj-smith47` |
| Repository | `anodizer` |
| Workflow filename | `release.yml` |
| Environment | *(leave blank unless your job uses one)* |

Once configured, anodizer detects the OIDC context (the GitHub-injected `ACTIONS_ID_TOKEN_REQUEST_URL` / `ACTIONS_ID_TOKEN_REQUEST_TOKEN` env vars), writes a process-private `.npmrc` carrying **no token line**, and threads those OIDC request vars into the `npm publish` subprocess so the npm CLI performs the exchange itself. No secret is read or written.

### `NPM_TOKEN` fallback

A Trusted Publisher cannot be attached to a package that does not yet exist, so the **first** publish that creates the package needs a token. Set the `NPM_TOKEN` env var (an [automation token](https://docs.npmjs.com/creating-and-viewing-access-tokens)); anodizer writes a process-private `.npmrc` carrying `//registry.npmjs.org/:_authToken=$NPM_TOKEN` and passes `--userconfig <that .npmrc>` to `npm publish`. The token is never placed on the argv and the `.npmrc` is deleted after publish completes. A present token always takes precedence over OIDC — drop the secret once the package exists and the Trusted Publisher is configured, and subsequent releases run tokenless.

For a private registry (e.g. GitHub Packages):

```yaml
npms:
  - scope: "@anodize"
    metapackage: demo
    registry: "https://npm.pkg.github.com"
    access: restricted
```

## Scoped vs unscoped packages

Scoped packages (`@org/name`) on npmjs.org default to **restricted** access unless `access: public` is set. Without `access: public`, the first publish of a scoped package on a free npm account fails with a `403`. In `optional-deps` mode the per-platform packages are always scoped (`scope:` is required), so `access: public` is typically needed there.

## Rollback

Within the 72-hour window after publishing, `anodize publish --rollback-only` runs `npm unpublish <name>@<version> --force` for each recorded target (every per-platform package and the metapackage in `optional-deps` mode). Outside the window, npm refuses unpublish requests, and anodizer surfaces a warning pointing at `npm deprecate` as the remaining remediation surface.

## Common gotchas

- **A credential is required for non-dry-run publishes.** Anodizer hard-errors when neither `NPM_TOKEN` nor a GitHub Actions OIDC context is present and `--dry-run` is not active. The error names both paths. It never publishes anonymously.
- **`access: public` required for scoped packages.** Without it, the first publish to a free npm account fails with `403`.
- **`scope:` required in `optional-deps` mode.** The per-platform packages need a scope; anodizer hard-errors when it is unset.
- **72-hour unpublish window.** After that, the version is permanent. The `required: true` default exists to give you a chance to spot a bad release before that window closes.

## Conditional publishing

Use `if` to gate publishing on a templated condition:

```yaml
npms:
  - scope: "@anodize"
    metapackage: demo
    if: "{{ Prerelease != \"\" }}"  # only on prereleases
```

## Full example

```yaml
npms:
  - id: primary
    scope: "@anodize"
    metapackage: anodize-demo
    bin: demo
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
```
