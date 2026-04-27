+++
title = "Chocolatey"
description = "Publish to the Chocolatey Windows package manager"
weight = 5
template = "docs.html"
+++

Anodizer generates Chocolatey `.nuspec` manifests and `chocolateyInstall.ps1` PowerShell scripts, packs them into `.nupkg` files, and pushes them to the [Chocolatey](https://chocolatey.org/) community repository (or a custom source). Chocolatey is the leading package manager for Windows, letting users install your tool with `choco install myapp`.

## Minimal config

```yaml
crates:
  - name: myapp
    publish:
      chocolatey:
        project_repo:
          owner: myorg
          name: myapp
        license: MIT
```

## Chocolatey config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | crate name | Override the Chocolatey package name |
| `ids` | string[] | none | Build ID filter: only include artifacts whose `id` is in this list |
| `project_repo.owner` | string | **required** | GitHub owner of the project repository |
| `project_repo.name` | string | **required** | GitHub repository name |
| `package_source_url` | string | none | URL shown as the package source in the Chocolatey gallery |
| `owners` | string | none | Chocolatey gallery package owner username |
| `title` | string | package name | Display title in the gallery (supports templates) |
| `authors` | string | crate name | Author name(s) shown in the gallery |
| `project_url` | string | GitHub repo URL | Project homepage URL |
| `url_template` | string | release URL | Custom URL template for download URLs (overrides the release asset URL) |
| `icon_url` | string | none | URL to the package icon image |
| `copyright` | string | none | Copyright notice (supports templates) |
| `description` | string | crate name | Package description (supports templates) |
| `license` | string | none | SPDX license identifier (e.g., `MIT`, `Apache-2.0`) |
| `license_url` | string | auto | Explicit license URL. Falls back to `https://opensource.org/licenses/<license>` |
| `require_license_acceptance` | bool | `false` | Require users to accept the license before install |
| `project_source_url` | string | none | Source code repository URL |
| `docs_url` | string | none | Documentation URL |
| `bug_tracker_url` | string | none | Bug tracker URL |
| `tags` | string or string[] | package name | Space-separated string or array of tags for the gallery |
| `summary` | string | none | Short summary of the package (supports templates) |
| `release_notes` | string | none | Release notes for this version (supports templates) |
| `dependencies` | object[] | none | Package dependencies (see below) |
| `api_key` | string | `$CHOCOLATEY_API_KEY` | Chocolatey API key for `choco push` (supports templates) |
| `source_repo` | string | `https://push.chocolatey.org/` | Push source URL |
| `skip_publish` | bool | `false` | Skip pushing the `.nupkg` to the Chocolatey repository |
| `disable` | bool or string | `false` | Disable this publisher entirely. Accepts a bool or a template string that evaluates to a truthy value |
| `use` | string | `archive` | Artifact type to package: `archive`, `msi`, or `nsis` |
| `amd64_variant` | string | `v1` | amd64 microarchitecture variant filter (`v1`, `v2`, `v3`, `v4`) |

### Dependencies

Each entry in the `dependencies` array has:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Chocolatey package ID of the dependency |
| `version` | string | Optional version constraint (e.g., `[1.0.0,)`) |

## API key setup

Anodizer needs a Chocolatey API key to push packages. You can provide it in two ways:

1. **Environment variable** (recommended for CI): set `CHOCOLATEY_API_KEY`.
2. **Config field**: set `api_key` in the chocolatey config. This field supports template rendering, so you can reference environment variables or other context values.

The environment variable is used as a fallback when `api_key` is not set in the config. To obtain an API key, sign in to [chocolatey.org/account](https://community.chocolatey.org/account) and generate one from your account page.

## How nuspec and nupkg files are generated

When the publish stage runs for Chocolatey, Anodizer:

1. **Finds Windows artifacts** from the build stage, filtering by `ids` and `amd64_variant` if configured. It looks for both 32-bit (i686/i386/x86) and 64-bit artifacts.
2. **Generates a `.nuspec` XML manifest** containing all package metadata (name, version, authors, description, license, tags, dependencies, etc.). All XML special characters are properly escaped.
3. **Generates a `chocolateyInstall.ps1` PowerShell script** placed in a `tools/` directory. The script uses `Install-ChocolateyZipPackage` with SHA-256 checksums. If both 32-bit and 64-bit artifacts are found, a dual-architecture script is generated that passes both URLs; otherwise a single-architecture script is produced.
4. **Runs `choco pack`** to create the `.nupkg` file from the nuspec and tools directory.
5. **Runs `choco push`** to upload the `.nupkg` to the configured source repository (defaults to `https://push.chocolatey.org/`).

If no Windows artifacts are found, Anodizer falls back to a placeholder GitHub release download URL and logs a warning.

## skip_publish behavior

When `skip_publish: true` is set, Anodizer skips the entire publish function early -- no nuspec is generated, no `choco pack` is run, and no push occurs. This is useful when you want to define the Chocolatey config for future use without actually publishing, or when another system handles the push step.

In dry-run mode (`--dry-run`), Anodizer logs what it would do without generating any files or running any commands.

## Full example

```yaml
crates:
  - name: myapp
    publish:
      chocolatey:
        name: myapp
        project_repo:
          owner: myorg
          name: myapp
        title: "My App"
        authors: "My Org"
        description: "A fast CLI tool for doing things"
        license: MIT
        project_url: "https://github.com/myorg/myapp"
        icon_url: "https://raw.githubusercontent.com/myorg/myapp/main/icon.png"
        copyright: "Copyright 2026 My Org"
        tags:
          - cli
          - tool
          - devops
        summary: "A fast CLI tool"
        docs_url: "https://myorg.github.io/myapp"
        bug_tracker_url: "https://github.com/myorg/myapp/issues"
        project_source_url: "https://github.com/myorg/myapp"
        dependencies:
          - id: dotnet-runtime
            version: "[6.0,)"
        source_repo: "https://push.chocolatey.org/"
        skip_publish: false
```
