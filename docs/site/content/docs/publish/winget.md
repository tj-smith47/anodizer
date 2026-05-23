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
        license: MIT
        package_identifier: "MyOrg.MyApp"
```

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
| `license` | string | **required** | SPDX license identifier (e.g. `MIT`) |
| `license_url` | string | none | License URL |
| `short_description` | string | `description` or crate name | Short description (max 256 chars) |
| `description` | string | none | Full package description |
| `homepage` | string | none | Project homepage URL |
| `url_template` | string | release URL | Custom download URL template |
| `ids` | list of strings | all | Build IDs filter: only include matching artifacts |
| `skip_upload` | bool or string | `false` | Skip publishing (`true` always skips, `"auto"` skips for prereleases) |
| `commit_msg_template` | string | `New version: {{ PackageIdentifier }} {{ Version }}` | Custom commit message template |
| `path` | string | auto-generated | Custom manifest path inside the repo |
| `release_notes` | string | none | Release notes for this version |
| `release_notes_url` | string | none | URL to full release notes |
| `installation_notes` | string | none | Post-install notes shown to the user |
| `tags` | list of strings | none | Tags for package discovery (lowercased, spaces replaced with hyphens) |
| `dependencies` | list of objects | none | Package dependencies (see below) |
| `product_code` | string | none | Product code for Add/Remove Programs |
| `use` | string | `archive` | Artifact type: `archive`, `msi`, or `nsis` |
| `amd64_variant` | string | `v1` | amd64 microarchitecture variant filter (`v1`, `v2`, `v3`, `v4`) |
| `update_existing_pr` | bool or string | `false` | Force-push to an existing open PR branch instead of skipping. See [Existing PR behavior](#existing-pr-behavior). |

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
        license: MIT                       # required; SPDX
        license_url: ""
        short_description: ""             # max 256 chars
        description: ""
        homepage: ""
        url_template: ""
        ids: []
        skip_upload: false               # bool | "auto" | template
        commit_msg_template: ""
        path: ""                         # custom manifest path in repo
        release_notes: ""
        release_notes_url: ""
        installation_notes: ""
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

The token can also be set via `repository.token` in the config. Falls back to `ANODIZER_FORCE_TOKEN` if `GITHUB_TOKEN` is not set.

## Common gotchas

- **Non-zip archives**: WinGet only accepts `.zip` format for archive installers. `.tar.gz`, `.7z`, and other formats are rejected with a clear error.
- **Mixed artifact types**: cannot mix zip archives and portable binaries in the same manifest. Anodizer errors if both types are detected.
- **Validation pipeline lag**: WinGet PR validation runs in the `microsoft/winget-pkgs` CI pipeline, which can take hours to complete. Rollback is warn-only — the PR cannot be cancelled programmatically once validation has started.
- **Duplicate PRs**: if a prior run pushed a PR for the same tag, use `update_existing_pr: true` to force-push the updated manifests instead of opening a second PR.

## Republish / update behavior

Set `update_existing_pr: true` to force-push an updated manifest to an existing open PR branch (using `--force-with-lease`) rather than skipping. This handles the case where a prior release attempt pushed stale manifests.

Not applicable for version replacement — each version requires a separate PR. To re-submit a rejected version, open a new PR (bump the version or fix the manifest).

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

## Generated manifests

Anodizer generates the WinGet 3-file manifest format:

- **`PackageId.yaml`** -- Version manifest declaring the package identifier, version, and default locale.
- **`PackageId.installer.yaml`** -- Installer manifest with download URLs, SHA-256 checksums, architecture mappings, and upgrade behavior. For `.zip` archives, nested installer entries map each binary as a portable executable.
- **`PackageId.locale.en-US.yaml`** -- Default locale manifest with publisher info, descriptions, license, tags, release notes, and other metadata.

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
