+++
title = "Homebrew Casks"
description = "Generate Homebrew cask formulae for macOS applications"
weight = 85
template = "docs.html"
+++

Anodizer can generate Homebrew Cask `.rb` files for macOS applications and push them to your tap repository. This is separate from [Homebrew formulas](/docs/publish/homebrew/) which are for CLI tools.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | false | re-clone tap, `git revert HEAD --no-edit`, push | `GITHUB_TOKEN contents:write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## Minimal config

```yaml
homebrew_casks:
  - name: myapp
    repository:
      owner: myorg
      name: homebrew-tap
```

## Full config reference

```yaml
homebrew_casks:
  - name: myapp                      # optional; cask name (default: project name)
    repository:
      owner: myorg                   # required
      name: homebrew-tap             # required
      token: "{{ .Env.GITHUB_TOKEN }}"  # optional; falls back to GITHUB_TOKEN env
      branch: main                   # optional; target branch
      pull_request:
        enabled: false               # optional; open PR instead of direct push
        base: main
    directory: Casks                 # optional; directory in the tap repo
    description: ""                  # optional
    homepage: ""                     # optional
    license: ""                      # optional
    app: ""                          # optional; app stanza name
    binaries: []                     # optional; binaries to symlink
    manpages: []                     # optional
    caveats: ""                      # optional; post-install message
    service: ""                      # optional; service definition
    custom_block: ""                 # optional; raw Ruby inserted into cask
    alternative_names: []            # optional
    ids: []                          # optional; filter by build IDs
    skip_upload: false               # optional; "auto" skips prereleases
    commit_author:
      name: ""
      email: ""
    commit_msg_template: ""          # optional
    url:
      template: ""                   # optional; download URL template
      verified: ""                   # optional
      using: ""                      # optional; download strategy
    completions:
      bash: ""                       # optional; path to bash completion
      zsh: ""                        # optional
      fish: ""                       # optional
    uninstall:
      quit: []
      delete: []
      launchctl: []
    zap:
      trash: []
    hooks:
      pre:
        install: ""
      post:
        install: ""
    dependencies:                    # optional
      - formula: cmake
      - cask: xquartz
    conflicts:                       # optional
      - cask: another-app
```

## Authentication

| Variable | Description |
|----------|-------------|
| `GITHUB_TOKEN` | Token with push access to your tap repository |

The token can also be set via `repository.token` in the config.

## Common gotchas

- **macOS only**: only macOS artifacts (`disk_image` or `archive` kind) are included. Linux/Windows targets are ignored.
- **SHA256 required**: cask files require a checksum on every artifact. Ensure the checksum stage runs before the cask publisher.
- **`update_existing_pr`**: if your tap uses a PR-based workflow, set `update_existing_pr: true` to force-push to an existing open branch rather than opening a duplicate PR. See [Cask existing PR behavior in the Homebrew doc](./homebrew.md#cask-existing-pr-behavior).

## Republish / update behavior

Not applicable as a config flag — cask files are updated in-place on each release. Re-cutting the same version overwrites the cask file in the tap (prior commit stays in git history). The Manager group rollback reverts via `git revert HEAD --no-edit` + push.

## Homebrew cask config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | project name | Cask name |
| `repository` | object | **required** | Tap repository (`owner`, `name`) |
| `directory` | string | `Casks` | Directory in the tap repo |
| `description` | string | none | Cask description |
| `homepage` | string | none | Homepage URL |
| `license` | string | none | License identifier |
| `app` | string | none | Application name for `app` stanza |
| `binaries` | list | none | Binaries to symlink |
| `manpages` | list | none | Man pages to install |
| `caveats` | string | none | Post-install caveats message |
| `service` | string | none | Service definition |
| `custom_block` | string | none | Raw Ruby inserted into the cask |
| `alternative_names` | list | none | Alternative cask names |
| `ids` | list | none | Filter by build IDs |
| `skip_upload` | string/bool | none | Skip git push (`"auto"` skips for prereleases) |
| `commit_author` | object | none | Git commit author (`name`, `email`) |
| `commit_msg_template` | string | auto-generated | Custom commit message (template) |

### URL config (`url`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `template` | string | auto-derived | Download URL template |
| `verified` | string | none | Verified domain for `verified:` stanza |
| `using` | string | none | Download strategy (e.g., `:homebrew_curl`) |
| `cookies` | map | none | HTTP cookies for the download |
| `referer` | string | none | Referer header |
| `headers` | list | none | Custom HTTP headers |
| `user_agent` | string | none | Custom user agent string |
| `data` | map | none | POST data for form submissions |

### Completions (`completions`)

| Field | Type | Description |
|-------|------|-------------|
| `bash` | string | Path to bash completion file |
| `zsh` | string | Path to zsh completion file |
| `fish` | string | Path to fish completion file |

### Uninstall / Zap (`uninstall`, `zap`)

| Field | Type | Description |
|-------|------|-------------|
| `launchctl` | list | Launch agent/daemon identifiers to stop |
| `quit` | list | Application bundle IDs to quit |
| `login_item` | list | Login item names to remove |
| `delete` | list | File paths to delete |
| `trash` | list | File paths to trash (preserves app state) |

### Hooks (`hooks`)

```yaml
hooks:
  pre:
    install: "system_command '/usr/bin/some-setup'"
    uninstall: "system_command '/usr/bin/some-cleanup'"
  post:
    install: "system_command '/usr/bin/post-setup'"
    uninstall: "system_command '/usr/bin/post-cleanup'"
```

### Generated completions (`generate_completions_from_executable`)

| Field | Type | Description |
|-------|------|-------------|
| `executable` | string | Binary to generate completions from |
| `args` | list | Arguments to pass to the executable |
| `base_name` | string | Base name for completion files |
| `shell_parameter_format` | string | Completion framework type (arg, clap, cobra, etc.) |
| `shells` | list | Target shells (bash, zsh, fish, pwsh) |

### Dependencies (`dependencies`)

Each entry can specify either `cask` or `formula`:

```yaml
dependencies:
  - formula: cmake
  - cask: xquartz
```

### Conflicts (`conflicts`)

Each entry can specify either `cask` or `formula`:

```yaml
conflicts:
  - cask: another-app
```

## Behavior

- Looks for macOS artifacts (`disk_image` or `archive` kind)
- Requires SHA256 checksum metadata on the artifact
- Clones the tap repository, writes the cask file, commits, and pushes
- Default commit message: `"Brew cask update for {{ .ProjectName }} version {{ .Tag }}"`

## Full example

```yaml
homebrew_casks:
  - name: myapp
    repository:
      owner: myorg
      name: homebrew-tap
    directory: Casks
    description: "My awesome application"
    homepage: "https://example.com/myapp"
    license: MIT
    app: "MyApp.app"
    uninstall:
      quit:
        - com.myorg.myapp
      delete:
        - "/Applications/MyApp.app"
    zap:
      trash:
        - "~/Library/Preferences/com.myorg.myapp.plist"
```
