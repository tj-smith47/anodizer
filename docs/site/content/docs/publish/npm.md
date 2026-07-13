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

### Custom per-platform package names

By default the per-platform packages are named `<scope>/<bin>-<os>-<cpu>[-<libc>]` using npm's tokens. `platform_name_template` replaces that entire name with a rendered template — the whole package name, per platform. With a template set, `scope:` becomes optional: a rendered name with no leading `@` is published unscoped (and validated as a legal npm name), or prefixed with `scope` when one is configured.

Beyond the standard release template context, four platform variables are available (values shown for concrete targets):

| Variable | Source | `x86_64-unknown-linux-musl` | `aarch64-apple-darwin` | `x86_64-pc-windows-msvc` |
|----------|--------|------------------------------|------------------------|--------------------------|
| `NpmOs` | npm `os` selector | `linux` | `darwin` | `win32` |
| `NpmCpu` | npm `cpu` selector | `x64` | `arm64` | `x64` |
| `NpmLibc` | npm `libc` selector | `musl` | *(empty)* | *(empty)* |
| `Os` | anodizer target mapping | `linux` | `darwin` | `windows` |
| `Arch` | anodizer target mapping | `amd64` | `arm64` | `amd64` |

Use `Os` when you want `windows` in the name (git-cliff-style) rather than npm's `win32`:

```yaml
npms:
  - metapackage: myapp
    bin: myapp
    libc_aware: false
    platform_name_template: "myapp-{{ Os }}-{{ NpmCpu }}"
```

```
myapp-linux-x64        package.json: { name: myapp-linux-x64,    os:[linux],  cpu:[x64] }
myapp-darwin-arm64     package.json: { name: myapp-darwin-arm64, os:[darwin], cpu:[arm64] }
myapp-windows-x64      package.json: { name: myapp-windows-x64,  os:[win32],  cpu:[x64] }
```

The npm `os`/`cpu`/`libc` selector **fields** inside each `package.json` always keep npm's tokens (`win32`, not `windows`) regardless of the name template — the template shapes only the package *name*, never the platform resolution.

If the template renders the same name for two distinct platforms — the classic case is omitting `{{ NpmLibc }}` while `libc_aware: true` keeps musl and glibc separate — the publisher fails with a config error naming the colliding packages:

```
npm: platform_name_template renders the same package name for multiple
platforms: myapp-linux-x64 — include enough platform vars (NpmOs / NpmCpu /
NpmLibc) to make every per-platform name unique
```

`platform_name_template` applies to `optional-deps` mode only; setting it in `postinstall` mode is a hard error.

### Publishing platform packages only

`skip_metapackage` emits and publishes **only** the per-platform packages — no metapackage, no `optionalDependencies` aggregate, no `bin` shim. Use it when the base npm package is hand-maintained — e.g. a TypeScript library that owns the package name and lists the binary packages under its own `optionalDependencies` — while anodizer owns building and publishing the per-platform binary packages:

```yaml
npms:
  - scope: "@myapp"
    bin: myapp
    access: public
    skip_metapackage: true
```

This publishes `@myapp/myapp-linux-x64-musl`, `@myapp/myapp-darwin-arm64`, … and nothing else; the hand-written base package references them itself.

Like `skip`, the field also accepts a template string, so a single config can gate the metapackage per release:

```yaml
npms:
  - scope: "@myapp"
    skip_metapackage: "{{ .IsSnapshot }}"   # snapshots publish platform packages only
```

`skip_metapackage` applies to `optional-deps` mode only; setting it in `postinstall` mode (which has no metapackage) is a hard error.

### Publishing a subset of targets

A project may build more targets than it publishes to npm. `targets:` restricts this publisher to a subset of the built target triples — only artifacts whose triple is listed become packages; the rest are silently skipped (a target left out of scope is *not* the same as a target with no npm `os`/`cpu` mapping, which is warned about). It is orthogonal to `ids:` — both filters apply.

For example, git-cliff builds twelve targets but ships npm for six:

```yaml
npms:
  - metapackage: git-cliff
    platform_name_template: "git-cliff-{{ Os }}-{{ NpmCpu }}"
    libc_aware: false
    targets:
      - x86_64-unknown-linux-gnu
      - aarch64-unknown-linux-gnu
      - x86_64-pc-windows-msvc
      - aarch64-pc-windows-msvc
      - x86_64-apple-darwin
      - aarch64-apple-darwin
```

A listed triple that no selected build produces is a config error naming the offending triple, so a typo fails preflight instead of silently narrowing the publisher to nothing.

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
| `scope` | string | none | npm scope for per-platform packages (`optional-deps`; required unless `platform_name_template` is set) |
| `metapackage` | string | `name`/crate name | Metapackage name users install (`optional-deps`) |
| `bin` | string | metapackage basename | Command name the metapackage installs (`optional-deps`) |
| `libc_aware` | bool | `true` | Emit linux musl/glibc as separate packages (`optional-deps`) |
| `platform_name_template` | string | none | Full-name template for per-platform packages (`optional-deps` only). See [Custom per-platform package names](#custom-per-platform-package-names) |
| `skip_metapackage` | string/bool | none | Publish only the per-platform packages; no metapackage (`optional-deps` only, templated like `skip`). See [Publishing platform packages only](#publishing-platform-packages-only) |
| `id` | string | none | Unique identifier (for `--id=...` selection) |
| `ids` | list | none | Filter artifacts by build ID (matches `crate_name`) |
| `targets` | list | all built | Target-triple allowlist: publish only these triples (`optional-deps` + `postinstall`). See [Publishing a subset of targets](#publishing-a-subset-of-targets) |
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
| `auth` | `auto` \| `token` \| `oidc` | `auto` | Credential-selection strategy, evaluated **per published package**. See [Authentication](#authentication). |
| `extra_files` | list | `[README*, LICENSE*]` | Glob set of files to include |
| `templated_extra_files` | list | none | Template-rendered file mappings (`{src, dst}`) |
| `extra` | map | none | Free-form root-level `package.json` fields (shallow-merged) |
| `skip` | string/bool | none | Skip this publisher (template-conditional; legacy `disable:` spelling accepted as an alias) |
| `if` | string | none | Template condition; skip if result is falsy |
| `required` | bool | `true` | Whether failure here aborts the release |

## Authentication

Anodizer authenticates to npm in one of two ways — a long-lived **token** (`NPM_TOKEN` / `cfg.token`) or **Trusted Publishing (OIDC)** — and chooses between them **per published package**. It never publishes anonymously: with neither credential available it hard-errors.

The `auth` field selects the strategy:

| `auth` | Behaviour |
|--------|-----------|
| `auto` (default) | Decide per package by probing the registry for the package's existence. An **existing** package prefers OIDC when an OIDC context is present (else the token); a **brand-new** package always uses the token (Trusted Publishing cannot create a non-existent package). On a failed OIDC publish, `auto` falls back to the token — see [OIDC failure fallback](#oidc-failure-fallback). |
| `token` | Always use the token; never attempt OIDC. Errors if no token is set. The historical behaviour. |
| `oidc` | Always use OIDC; never fall back to a token. Errors if no OIDC context is present. A failed exchange fails the release loudly. |

### Why per-package selection matters

In `optional-deps` mode a single `npms[]` entry publishes a **metapackage plus one package per platform**. The metapackage often already exists (with a Trusted Publisher configured) while the per-platform sub-packages are brand new on a given release. With `auth: auto` and `NPM_TOKEN` set, anodizer publishes the **new sub-packages via the token** (Trusted Publishing cannot create them) and the **existing metapackage via OIDC** — in one run, no per-package config:

```yaml
npms:
  - scope: "@anodize"
    metapackage: demo
    auth: auto      # default — per-package selection
```

Keep `NPM_TOKEN` set in the workflow; `auto` exercises Trusted Publishing wherever a package already exists and a Trusted Publisher is configured, and uses the token only where it must.

### OIDC failure fallback

In `auto` mode only, when OIDC is chosen for an existing package and the `npm publish` **fails**, and a token is available, anodizer **retries that package with the token** and emits a loud warning naming the package:

```
WARN  OIDC / Trusted Publishing publish FAILED for '@anodize/demo'; falling back to
      NPM_TOKEN — Trusted Publishing was NOT exercised for this package. Verify the
      package's Trusted Publisher config (registry, repository, workflow).
```

The release succeeds via the token, but the operator clearly sees that Trusted Publishing did not work for that package and can fix its Trusted Publisher config. In `oidc` mode there is **no fallback** — a failed exchange fails the release. In `token` mode OIDC is never attempted.

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

### Provenance needs a GitHub-hosted runner

npm provenance attestations are produced through GitHub's OIDC provenance flow, which
npm only accepts from a **GitHub-hosted** runner. On a self-hosted runner the provenance
exchange is unavailable, and npm rejects a publish that requests it.

This github-hosted requirement is specific to **npm's provenance policy**, not a property
of GitHub Actions OIDC in general. GitHub mints a valid OIDC id-token on self-hosted
runners too; npm's provenance verifier is what rejects the `self-hosted` runner-environment
claim. Anodizer's other OIDC-authenticated publisher — the [MCP registry](./mcp-registry.md)
(`auth.type: github-oidc`) — runs on a self-hosted runner without issue, because that
registry verifies repository ownership (issuer, audience, and `repository_owner`), not the
runner environment. So only npm needs the separate github-hosted job.

Anodizer degrades gracefully: when it detects that provenance cannot be produced on the
current runner, it publishes **without** provenance and emits a warning rather than
failing the release. The package still ships; only the provenance attestation is absent.

To keep provenance, run npm on a separate GitHub-hosted job. Anodizer's own release does
exactly this — and peels **every** publisher that authenticates from a GitHub Actions
OIDC identity onto that one hosted job: npm (provenance) and pypi ([Trusted
Publishing](./pypi.md#trusted-publishing-oidc)). The main publish runs on a self-hosted
runner with `--skip=npm,pypi`, and a small github-hosted job runs the complementary
`--publishers npm,pypi`:

```yaml
jobs:
  publish:                       # self-hosted: everything except the OIDC publishers
    runs-on: self-hosted
    steps:
      - run: anodizer release --publish-only --skip=npm,pypi

  publish-oidc:                  # github-hosted: npm provenance + PyPI Trusted Publishing
    needs: publish
    runs-on: ubuntu-latest
    permissions:
      id-token: write            # mints the npm provenance + PyPI upload token
    env:
      NPM_TOKEN: ${{ secrets.NPM_TOKEN }}   # first-publish fallback; pypi under auth: oidc needs no token
    steps:
      - run: anodizer release --publish-only --publishers npm,pypi
```

The `--publishers`/`--skip` selectors that make this split possible are described in
[Selecting publishers](./selecting-publishers.md).

### `NPM_TOKEN` fallback

A Trusted Publisher cannot be attached to a package that does not yet exist, so the **first** publish that creates the package needs a token. Set the `NPM_TOKEN` env var (an [automation token](https://docs.npmjs.com/creating-and-viewing-access-tokens)); anodizer writes a process-private `.npmrc` carrying `//registry.npmjs.org/:_authToken=$NPM_TOKEN` and passes `--userconfig <that .npmrc>` to `npm publish`. The token is never placed on the argv and the `.npmrc` is deleted after publish completes.

Under the default `auth: auto`, a token is used for **brand-new** packages and as the credential when no OIDC context is present; **existing** packages prefer OIDC when one is. So you can keep `NPM_TOKEN` set permanently and still exercise Trusted Publishing wherever a package already exists — there is no need to drop the secret. To force token-only auth regardless of existence, set `auth: token`.

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

## Artifact eligibility

npm has the broadest OS coverage of any anodizer publisher — unlike the
macOS+Linux-only aggregators (Homebrew, nix, krew, AUR), it packages an archive
for every OS npm's `os` field represents: `linux`, `darwin` (genuine macOS
only), `win32` (Windows), `freebsd`, `openbsd`, `netbsd`, `aix`, and `android`.
Each built target's npm `os`/`cpu`/`libc` triple is derived automatically from
its real target triple (see [Quick start](#quick-start-optional-deps) above); a
target npm has no mapping for at all — an unmapped arch, or `darwin-universal`
(npm has no universal-arch selector) — is excluded from npm coverage with a
warning rather than silently dropped.

Apple **non-macOS** targets (`aarch64-apple-ios`, `*-tvos`, `*-watchos`) are the
one systematic exclusion: npm has no `ios` platform value, and a `darwin`-tagged
package built from a watchOS/tvOS archive would be wrongly selected by `npm
install` on a real macOS host. They never appear in the generated package or its
optional-dependency set.

If the build produces **no** eligible archive at all, anodizer fails the release
rather than emitting an empty package. See
[Artifact eligibility](./selecting-publishers.md#artifact-eligibility) for how
npm's coverage compares to the other install aggregators.
