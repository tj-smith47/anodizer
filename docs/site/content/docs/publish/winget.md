+++
title = "Winget"
description = "Publish to the Windows Package Manager (winget)"
weight = 6
template = "docs.html"
+++

Anodizer generates [WinGet](https://learn.microsoft.com/en-us/windows/package-manager/) YAML manifests and submits pull requests to the winget-pkgs community repository (or your own fork) via the GitHub API. WinGet is the official Windows Package Manager, allowing users to install your tool with `winget install Publisher.AppName`.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Submitter | false | warn-only (manual PR close against `microsoft/winget-pkgs`; upstream validation cannot be cancelled mid-flight) | `GITHUB_TOKEN pull_request:write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## The `required:` field

Default: **`false`** — a winget PR submission failure is logged but does not fail the release.

Set `required: true` to make the release exit non-zero if this publisher fails:

```yaml
crates:
  - name: myapp
    publish:
      winget:
        package_identifier: "MyOrg.MyApp"
        publisher: "My Organization"
        license: MIT
        required: true
```

> **Warning:** Winget is a _submitter_ publisher — it opens a pull request against
> `microsoft/winget-pkgs`; that PR goes through automated validation (SubmitPipelineBot)
> that takes hours to days. The publisher "succeeds" when the PR is opened, not when
> it is merged.
>
> Setting `required: true` therefore has no meaningful effect: the failure mode it
> guards against (PR rejection) happens asynchronously, long after the release exits.
>
> Anodizer emits this warning at config-validation time when `required: true` is set:
>
> ```
> <location>: publisher 'winget' is a submitter (external moderation queue); `required: true` has no meaningful effect — the submitter gate evaluates at push time, not at approval time.
> ```
>
> See [Publish overview — the `required:` field](../) for the full submitter-publisher
> semantics.

## Minimal config

```yaml
crates:
  - name: myapp
    publish:
      winget:
        repository:
          owner: myorg
          name: winget-pkgs
        publisher: "My Organization"
        package_identifier: "MyOrg.MyApp"
```

`license` is omitted here — it derives from the crate's `Cargo.toml`
`[package].license`. Set `publish.winget.license` only to override it (or if
the crate has no SPDX `license`, since winget manifests require one).

## How it works

1. Anodizer collects your Windows `.zip` archive artifacts (or portable binary artifacts).
2. It generates three YAML manifest files following the WinGet 1.12.0 schema.
3. The manifests are committed to a branch in your fork of the winget-pkgs repository.
4. A pull request is submitted against `microsoft/winget-pkgs` (or a custom upstream).

## Package identifier format

The `package_identifier` must follow the WinGet convention: 2 to 8 dot-separated segments, where each segment contains no whitespace or special characters (`\ / : * ? " < > |`).

Examples of valid identifiers:
- `MyOrg.MyApp`
- `Publisher.Category.AppName`

If `package_identifier` is not set, Anodizer auto-generates it as `Publisher.Name` (with spaces stripped from the publisher name).

## WinGet config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | crate name | Override the package name |
| `package_name` | string | same as `name` | Display name shown in WinGet gallery |
| `package_identifier` | string | `Publisher.Name` | WinGet package identifier (e.g. `Publisher.AppName`) |
| `publisher` | string | repo owner | Publisher name (required) |
| `publisher_url` | string | none | Publisher homepage URL |
| `publisher_support_url` | string | none | Publisher support URL |
| `privacy_url` | string | none | Privacy policy URL |
| `author` | string | none | Author name |
| `copyright` | string | none | Copyright notice |
| `copyright_url` | string | none | Copyright URL |
| `license` | string | Cargo `[package].license` | SPDX license identifier (e.g. `MIT`). Derived from `Cargo.toml`; winget manifests require a license, so set this if the crate has none. |
| `license_url` | string | none | License URL |
| `short_description` | string | `description` | Short description (max 256 chars). Falls back to the (Cargo-derived) description. |
| `description` | string | Cargo `[package].description` | Full package description. Derived from `Cargo.toml`; set to override. |
| `homepage` | string | Cargo `[package].homepage` | Project homepage URL. Derived from `Cargo.toml`; set to override. |
| `default_locale` | string | `en-US` | Manifest locale: stamped into the version manifest (`DefaultLocale`), installer manifest (`InstallerLocale`), locale manifest (`PackageLocale`), and the `.locale.<locale>.yaml` file name. Templates allowed. |
| `url_template` | string | release URL | Custom download URL template |
| `ids` | list of strings | all | Build IDs filter: only include matching artifacts |
| `skip_upload` | bool or string | `false` | Skip publishing (`true` always skips, `"auto"` skips for prereleases) |
| `commit_msg_template` | string | `New version: {{ PackageIdentifier }} {{ Version }}` | Custom commit message template |
| `path` | string | auto-generated | Custom manifest path inside the repo |
| `release_notes` | string | none | Release notes for this version |
| `release_notes_url` | string | none | URL to full release notes |
| `installation_notes` | string | none | Post-install notes shown to the user |
| `documentations` | list of objects | none | Documentation links shown in the WinGet gallery — each entry is `{ label, url }`. See [Documentation links](#documentation-links). |
| `moniker` | string | binary name | Short invoke alias surfaced as `Moniker` in the locale manifest, e.g. `winget install --id <id>` users can later run `winget show <moniker>`. Auto-derives from the binary name; set to override. |
| `upgrade_behavior` | string | `install` | Installer upgrade behavior written to the installer manifest: `install` (overlay the new version), `uninstallPrevious`, or `deny`. Portable-zip tools want `install`. |
| `silent_switch` | string | none | Silent-install switch string for `msi`/`exe` installers (e.g. `/S`, `/quiet`). Only meaningful when `use: msi`/`use: nsis`; portable-zip packages need no switch. |
| `tags` | list of strings | none | Tags for package discovery (lowercased, spaces replaced with hyphens) |
| `dependencies` | list of objects | none | Package dependencies (see below) |
| `product_code` | string | none | Product code for Add/Remove Programs |
| `use` | string | `archive` | Artifact type: `archive`, `msi`, or `nsis` |
| `amd64_variant` | enum | `v1` | amd64 microarchitecture variant filter — exactly one of `v1`, `v2`, `v3`, `v4` (any other value is rejected when the config is parsed) |
| `update_existing_pr` | bool or string | `false` | Force-push to an existing open PR branch instead of skipping. See [Existing PR behavior](#existing-pr-behavior) and [Recovery flags](../advanced/recovery-flags.md#update_existing_pr-winget-krew-homebrew-cask). |

## Repository config

You can configure the target repository with either the legacy `manifests_repo` or the unified `repository` field. The `repository` field supports additional options like branch control, SSH access, and pull request settings.

### Legacy: `manifests_repo`

| Field | Type | Description |
|-------|------|-------------|
| `manifests_repo.owner` | string | GitHub owner of your winget-pkgs fork |
| `manifests_repo.name` | string | Repository name of your fork |

### Unified: `repository`

| Field | Type | Description |
|-------|------|-------------|
| `repository.owner` | string | Repository owner |
| `repository.name` | string | Repository name |
| `repository.token` | string | Auth token (falls back to env-based resolution) |
| `repository.branch` | string | Branch to push to (default: auto-generated as `PackageIdentifier-Version`) |
| `repository.git.url` | string | Git URL for SSH-based publishing |
| `repository.git.ssh_command` | string | Custom SSH command |
| `repository.git.private_key` | string | Path to SSH private key |
| `repository.pull_request.enabled` | bool | Enable PR creation |
| `repository.pull_request.draft` | bool | Create PR as draft |
| `repository.pull_request.body` | string | Body text for the PR |
| `repository.pull_request.base.owner` | string | Upstream repo owner to PR against |
| `repository.pull_request.base.name` | string | Upstream repo name to PR against |
| `repository.pull_request.base.branch` | string | Upstream base branch to target |

## Full config reference

```yaml
crates:
  - name: myapp
    publish:
      winget:
        name: ""                           # override package name
        package_name: ""                   # display name in gallery
        package_identifier: "Org.App"     # required; 2–8 dot-separated segments
        publisher: "My Org"               # required
        publisher_url: ""
        publisher_support_url: ""
        privacy_url: ""
        author: ""
        copyright: ""
        copyright_url: ""
        license: MIT                       # SPDX; derived from Cargo.toml license if omitted
        license_url: ""
        short_description: ""             # max 256 chars; derived from Cargo.toml description if omitted
        description: ""                   # derived from Cargo.toml description if omitted
        default_locale: en-US             # manifest locale (also names the .locale.<locale>.yaml file)
        homepage: ""                      # derived from Cargo.toml homepage if omitted
        url_template: ""
        ids: []
        skip_upload: false               # bool | "auto" | template
        commit_msg_template: ""
        path: ""                         # custom manifest path in repo
        release_notes: ""
        release_notes_url: ""
        installation_notes: ""
        documentations:                  # gallery links: { label, url } pairs
          - label: "Documentation"
            url: "https://example.com/docs"
        moniker: ""                      # short alias; derives from binary name
        upgrade_behavior: install        # install | uninstallPrevious | deny
        silent_switch: ""                # msi/exe installers only (InstallerSwitches.Silent)
        tags: []
        dependencies: []
        product_code: ""
        use: archive                     # archive | msi | nsis
        amd64_variant: v1                # v1 | v2 | v3 | v4
        update_existing_pr: false
        repository:
          owner: myorg
          name: winget-pkgs
          token: ""                      # falls back to GITHUB_TOKEN
          branch: ""
          pull_request:
            enabled: true
            base:
              owner: microsoft
              name: winget-pkgs
              branch: master
        commit_author:
          name: ""
          email: ""
```

## Authentication

| Variable | Description |
|----------|-------------|
| `GITHUB_TOKEN` | Token with push access to your winget-pkgs fork and `pull_request:write` scope for the upstream PR |

Anodizer resolves the token from the first source that is set, in this order: `repository.token` in the config, then the `ANODIZER_GITHUB_TOKEN` env var, then the `GITHUB_TOKEN` env var.

## Common gotchas

- **Non-zip archives**: WinGet only accepts `.zip` format for archive installers. `.tar.gz`, `.7z`, and other formats are rejected with a clear error.
- **Mixed artifact types**: cannot mix zip archives and portable binaries in the same manifest. Anodizer errors if both types are detected.
- **Validation pipeline lag**: WinGet PR validation runs in the `microsoft/winget-pkgs` CI pipeline, which can take hours to complete. Rollback is warn-only — the PR cannot be cancelled programmatically once validation has started.
- **Duplicate PRs**: if a prior run pushed a PR for the same tag, use `update_existing_pr: true` to force-push the updated manifests instead of opening a second PR.

## Commit author

| Field | Type | Description |
|-------|------|-------------|
| `commit_author.name` | string | Git commit author name |
| `commit_author.email` | string | Git commit author email |

## Dependencies

Each entry in the `dependencies` list has:

| Field | Type | Description |
|-------|------|-------------|
| `package_identifier` | string | WinGet package identifier of the dependency |
| `minimum_version` | string | Minimum required version (optional) |
| `architectures` | list of strings | Architecture scope (optional). When set, attaches the dependency **only** to installers whose architecture matches; values: `x64`, `arm64`, `x86`. Unset or empty = applies to every installer. |

A package built for several Windows architectures produces one installer entry
per architecture inside the installer manifest. By default a dependency is
manifest-wide: it lands on *every* installer entry. `architectures` scopes a
dependency to specific installers so an architecture-specific runtime only
attaches where it belongs.

The scope match is exact and case-sensitive against each installer's WinGet
architecture (`x64`/`arm64`/`x86`). A value outside that set matches no
installer, so config validation rejects it up front rather than silently
dropping the dependency from the generated manifest.

### Per-architecture runtime dependencies

The canonical case is the Microsoft Visual C++ redistributable, which ships as
separate `x64` and `arm64` packages. An MSVC/Rust binary that publishes both an
x64 and a native arm64 installer must scope each redistributable to its own
architecture:

```yaml
crates:
  - name: myapp
    publish:
      winget:
        dependencies:
          - package_identifier: "Microsoft.VCRedist.2015+.x64"
            architectures: ["x64"]
          - package_identifier: "Microsoft.VCRedist.2015+.arm64"
            architectures: ["arm64"]
          # Unscoped — attaches to every installer:
          - package_identifier: "Acme.CommonRuntime"
```

Without the `architectures:` scopes, the x64 redistributable would also be
declared as a dependency of the **arm64** installer (and vice versa), which is
wrong: an Apple-Silicon-class Arm device cannot satisfy a dependency on the x64
redistributable, and WinGet validation rejects (or, post-install, fails to
resolve) the cross-architecture dependency.

The per-installer `Dependencies` block is emitted into the installer manifest
(`PackageId.installer.yaml`). With the scoping above, each installer entry
declares only the dependencies that match its architecture:

```yaml
Installers:
- Architecture: x64
  InstallerUrl: https://.../myapp-x64.zip
  InstallerSha256: ...
  Dependencies:
    PackageDependencies:
    - PackageIdentifier: Microsoft.VCRedist.2015+.x64
    - PackageIdentifier: Acme.CommonRuntime
- Architecture: arm64
  InstallerUrl: https://.../myapp-arm64.zip
  InstallerSha256: ...
  Dependencies:
    PackageDependencies:
    - PackageIdentifier: Microsoft.VCRedist.2015+.arm64
    - PackageIdentifier: Acme.CommonRuntime
```

When no dependency matches a given installer's architecture, the
`Dependencies` key is omitted from that installer entry entirely.

## Documentation links

`documentations` adds a `Documentations` block to the locale manifest — the
WinGet gallery renders these as labeled links (docs, source, support). Each
entry is a `{ label, url }` pair; `label` becomes `DocumentLabel` and `url`
becomes `DocumentUrl`:

```yaml
crates:
  - name: myapp
    publish:
      winget:
        documentations:
          - label: "Documentation"
            url: "https://tj-smith47.github.io/anodizer"
          - label: "Source"
            url: "https://github.com/tj-smith47/anodizer"
```

renders into `PackageId.locale.en-US.yaml`:

```yaml
Documentations:
- DocumentLabel: Documentation
  DocumentUrl: https://tj-smith47.github.io/anodizer
- DocumentLabel: Source
  DocumentUrl: https://github.com/tj-smith47/anodizer
```

## Upgrade behavior, moniker, and silent switch

- **`upgrade_behavior`** is written to every installer entry as
  `UpgradeBehavior`. The default `install` overlays the new version, which is
  correct for a portable-zip tool (there is no installer-managed prior version
  to remove first). Use `uninstallPrevious` for MSI/EXE installers that manage
  their own uninstall, or `deny` to forbid in-place upgrades.

  ```yaml
  winget:
    upgrade_behavior: install   # default; portable-zip packages
  ```

- **`moniker`** auto-derives from the binary name and is emitted as `Moniker`
  in the locale manifest — the short alias users type with `winget show`. Set
  it only to override (e.g. a binary named `myapp-cli` published under the
  alias `myapp`).

- **`silent_switch`** is only meaningful for `use: msi` / `use: nsis` installer
  artifacts; it sets `InstallerSwitches.Silent`. For a portable-zip package it
  is ignored with a warning, since a zip has no installer to pass a switch to.

## Generated manifests

Anodizer generates the WinGet 3-file manifest format:

- **`PackageId.yaml`** -- Version manifest declaring the package identifier, version, and default locale.
- **`PackageId.installer.yaml`** -- Installer manifest with download URLs, SHA-256 checksums, architecture mappings, and upgrade behavior. For `.zip` archives, nested installer entries map each binary as a portable executable.
- **`PackageId.locale.<locale>.yaml`** -- Default locale manifest with publisher info, descriptions, license, tags, release notes, and other metadata. The locale defaults to `en-US`; set `default_locale` to change it.

Each file includes a YAML language server schema reference header and a generated-by-anodizer comment.

Manifests are placed at `manifests/<first-char>/<PackageId segments>/<version>/` inside the repository. For example, `TJSmith.Anodizer` version `1.0.0` would be written to `manifests/t/TJSmith/Anodizer/1.0.0/`. You can override this with the `path` field.

## Architecture mapping

Anodizer maps Rust target triples to WinGet architecture identifiers:

| Rust target | WinGet architecture |
|-------------|---------------------|
| `x86_64-pc-windows-*` | `x64` |
| `i686-pc-windows-*` | `x86` |
| `aarch64-pc-windows-*` | `arm64` |

Only Windows artifacts (detected by target triple or path) are included. Non-zip archives (tar.gz, 7z) are rejected -- WinGet requires `.zip` format for archive installers.

## Installer types

Anodizer supports two installer types:

- **zip** -- Archive artifacts containing portable executables. Each binary gets a `NestedInstallerFiles` entry with a `PortableCommandAlias`. If the archive wraps contents in a top-level directory, `RelativeFilePath` entries are prefixed accordingly.
- **portable** -- Bare binary artifacts. Each binary gets a `Commands` entry.

You cannot mix archive and portable binary artifacts in the same manifest. Anodizer will error if both types are found.

## Existing PR behavior

When `gh pr create` reports that a PR for the same head branch already exists,
Anodizer's default is to **skip and emit a warning**:

```
winget: PR for 'owner:MyOrg.MyApp-1.2.3' already exists — skipping
        (set update_existing_pr: true to update the PR in place)
```

Setting `update_existing_pr: true` force-pushes the updated manifest to the
existing branch using `--force-with-lease`, so the open PR automatically picks
up the new content:

```yaml
winget:
  update_existing_pr: true
```

This is useful when a prior run pushed a stale manifest (e.g. from a failed
release) and you want to replace it without closing and reopening the PR.

## skip_upload

The `skip_upload` field controls whether publishing is skipped:

- `false` (default) -- Always publish.
- `true` -- Always skip.
- `"auto"` -- Skip when the version is a prerelease (e.g. `1.0.0-rc1`).
- A template string -- Evaluated at runtime; if the result is `"true"`, publishing is skipped.

## Template rendering

All string fields support Tera template rendering. The commit message template receives two extra variables beyond the standard context:

- `{{ PackageIdentifier }}` -- The resolved package identifier.
- `{{ Version }}` -- The release version.

The default commit message is `New version: {{ PackageIdentifier }} {{ Version }}`.

## Full example

```yaml
crates:
  - name: myapp
    publish:
      winget:
        package_identifier: "MyOrg.MyApp"
        publisher: "My Organization"
        publisher_url: "https://myorg.example.com"
        license: MIT
        license_url: "https://github.com/myorg/myapp/blob/main/LICENSE"
        short_description: "A fast CLI tool for doing things"
        description: "A fast CLI tool for doing things, with detailed features and capabilities."
        homepage: "https://myorg.example.com/myapp"
        tags:
          - cli
          - devtools
        dependencies:
          - package_identifier: "Microsoft.VCRedist.2015+.x64"
            minimum_version: "14.0.0"
        release_notes_url: "https://github.com/myorg/myapp/releases/tag/{{ version }}"
        installation_notes: "Run 'myapp --help' to get started."
        commit_msg_template: "Update {{ PackageIdentifier }} to {{ Version }}"
        repository:
          owner: myorg
          name: winget-pkgs
          branch: "myapp-{{ version }}"
          pull_request:
            enabled: true
            base:
              owner: microsoft
              name: winget-pkgs
              branch: master
        commit_author:
          name: "Release Bot"
          email: "bot@myorg.example.com"
```
