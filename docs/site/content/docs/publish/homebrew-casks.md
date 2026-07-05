+++
title = "Homebrew Casks"
description = "Generate Homebrew cask formulae for macOS applications"
weight = 85
template = "docs.html"
+++

Anodizer generates Homebrew Cask `.rb` files for your macOS artifacts and pushes them to your tap repository. A cask is the canonical Homebrew channel for **pre-compiled binaries** — both GUI `.app` bundles (via the `app` stanza) and CLI tools (via a `binary` stanza that symlinks an executable onto the user's `PATH`). The [Homebrew Formula](/docs/publish/homebrew/) path — which compiles from source — is deprecated upstream for pre-built release binaries; use a cask. Set `binaries:` to ship a CLI; set `app:` to ship a GUI application; set both when a bundle also exposes a CLI.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | false | re-clone tap, `git revert HEAD --no-edit`, push | `GITHUB_TOKEN contents:write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## The `required:` field

Default: **`false`** — a Homebrew Cask push failure is logged but does not fail the release.

Set `required: true` to make the release exit non-zero if this publisher fails:

```yaml
homebrew_casks:
  - name: myapp
    repository:
      owner: myorg
      name: homebrew-tap
    required: true
```

See [Publish overview — the `required:` field](../) for the full semantics.

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
      token: "{{ Env.GITHUB_TOKEN }}"  # optional; falls back to GITHUB_TOKEN env
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
    livecheck:                       # optional; version polling (default: skip)
      strategy: github_latest        # optional; livecheck strategy symbol
      url: url                       # optional; :url shorthand or a literal URL
      regex: ""                      # optional; raw Ruby for page_match strategies
      skip: false                    # optional; true forces the skip stanza
      skip_reason: ""                # optional; custom skip message
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

Cask files are updated in-place on each release; no recovery flag is required for re-cuts. When a PR-based workflow is configured and a prior run left an open PR, set `update_existing_pr: true` to force-push the updated cask file instead of opening a duplicate. See [Cask existing PR behavior in the Homebrew doc](./homebrew.md#cask-existing-pr-behavior) and [Recovery flags](../advanced/recovery-flags.md#update-existing-pr-winget-krew-homebrew-cask) for the full mechanism.

## Homebrew cask config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | project name | Cask name |
| `repository` | object | **required** | Tap repository (`owner`, `name`) |
| `directory` | string | `Casks` | Directory in the tap repo |
| `description` | string | Cargo `[package].description` | Cask description. Derived from `Cargo.toml`; set to override. |
| `homepage` | string | Cargo `[package].homepage` | Homepage URL. Derived from `Cargo.toml`; set to override. |
| `license` | string | none | License identifier |
| `app` | string | none | Application name for `app` stanza |
| `binaries` | list | none | Binaries to symlink |
| `manpages` | list | none | Man pages to install |
| `caveats` | string | none | Post-install caveats message |
| `service` | string | none | Service definition |
| `custom_block` | string | none | Raw Ruby inserted into the cask |
| `livecheck` | object | skip | `livecheck do … end` stanza for version polling. See [Livecheck](#livecheck). |
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

### Livecheck (`livecheck`) {#livecheck}

By default the cask emits `livecheck do skip "Auto-generated on release." end` —
a binary cask's `url`/`sha256` are rewritten on every release, so there is
nothing stable for `brew livecheck` to poll. Set a `strategy` (and optionally
`url`/`regex`) to opt into active version detection. For a GitHub-released
project, `github_latest` against the cask's own `url` stanza (`:url`, the
idiomatic cask shorthand) is the right pairing:

```yaml
homebrew_casks:
  - name: myapp
    repository:
      owner: myorg
      name: homebrew-tap
    livecheck:
      strategy: github_latest
      url: url
```

renders into the cask:

```ruby
  livecheck do
    url :url
    strategy :github_latest
  end
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `strategy` | string | none | Livecheck strategy symbol (e.g. `github_latest`, `git`, `page_match`) |
| `url` | string | `url` | `:url` / `:homepage` symbol shorthand, or a literal URL string |
| `regex` | string | none | Raw Ruby regex (e.g. `%r{v(\d+\.\d+)}i`) for `page_match`-style strategies |
| `skip` | bool | auto | `true` forces the skip stanza; defaults to skip when no `strategy`/`url`/`regex` is set |
| `skip_reason` | string | `Auto-generated on release.` | Custom message for the skip stanza |

`url` accepts a Ruby symbol shorthand (`url` / `stable` / `head` / `homepage` →
`url :url`) or a literal URL string. Setting `skip: false` without any of
`strategy`/`url`/`regex` falls back to `skip` with a warning — an empty
`livecheck do … end` is invalid. anodizer's cask `livecheck` is fully
configurable (`strategy`, `url`, `regex`, `skip`), matching how the
overwhelming majority of real Homebrew casks declare version detection.

## Multi-architecture casks

When a release builds more than one macOS architecture (typically
`darwin/amd64` for Intel Macs plus `darwin/arm64` for Apple Silicon), anodizer
emits a cask body with per-architecture `on_intel` / `on_arm` stanzas. Each
stanza carries its own `url` and `sha256`, so `brew install` serves every Mac
host the binary built for its CPU:

```ruby
cask "myapp" do
  version "1.2.3"

  name "myapp"

  livecheck do
    skip "Auto-generated on release."
  end

  on_macos do
    on_arm do
      sha256 "2222222222222222222222222222222222222222222222222222222222222222"
      url "https://github.com/myorg/myapp/releases/download/v#{version}/myapp-darwin-arm64.tar.gz"
    end
    on_intel do
      sha256 "1111111111111111111111111111111111111111111111111111111111111111"
      url "https://github.com/myorg/myapp/releases/download/v#{version}/myapp-darwin-amd64.tar.gz"
    end
  end

  binary "myapp"
end
```

The version substring in each URL is rewritten to `#{version}` so Homebrew
auto-updates the download on the next release. The `url`/`verified`/`using` and
other [URL config](#url-config-url) values you set apply inside every per-arch
block.

This emission is automatic — there is no flag to enable it. anodizer decides the
shape from the artifacts present in the release:

- **Multiple macOS architectures** → per-arch `on_intel` / `on_arm` blocks (the
  shape above). The same mechanism handles Linux casks: a `darwin/amd64` +
  `darwin/arm64` + `linux/amd64` release produces an `on_macos` block (with two
  arch entries) *and* an `on_linux` block.
- **One macOS architecture** → a flat top-level `url` / `sha256`, with no
  `on_intel` / `on_arm` wrappers:

  ```ruby
  cask "myapp" do
    version "1.2.3"
    sha256 "1111111111111111111111111111111111111111111111111111111111111111"

    url "https://github.com/myorg/myapp/releases/download/v#{version}/myapp-darwin-arm64.tar.gz"

    name "myapp"
    # ...
    binary "myapp"
  end
  ```

Each OS×arch slot is filled by the first artifact found in **kind precedence
order**: `disk_image` (`.dmg`) > `archive` (`.tar.gz`/`.zip`) > uploadable
binary. The first kind that supplies a given slot wins, so a release that
produces both a `.dmg` and a `.tar.gz` per arch fills the cask from the `.dmg`.
Dedup is per-OS, so a macOS `intel` entry never suppresses a Linux `intel`
entry.

Every artifact filling a slot **must** carry `sha256` metadata: a cask block
with an empty `sha256 ""` line fails `brew style` and aborts `brew install`
(Homebrew verifies the digest before extracting), so a missing checksum is a
hard error rather than a degraded cask.

### Interaction with `universal_binaries`

The per-arch slots are always filled from the **real per-architecture** macOS
artifacts (`darwin/amd64` and `darwin/arm64`), never from a lipo'd universal
binary — a universal artifact has the synthetic `darwin-universal` target
(architecture `all`), which matches neither the `intel` nor the `arm` slot and
is skipped by the cask builder. What changes is whether the per-arch artifacts
still exist:

- **`universal_binaries.replace: false`** (or unset) → the per-arch `amd64` and
  `arm64` artifacts remain in the catalog alongside the universal binary, so the
  cask renders the full `on_intel` + `on_arm` multi-arch body above.
- **`universal_binaries.replace: true`** → the per-arch source artifacts are
  removed from the catalog once the universal binary is built. With both
  per-arch macOS slots gone, the cask falls back to the single-arch flat
  `url` / `sha256` shape, served from whatever single non-universal macOS
  artifact remains.

So if you want a multi-arch cask with explicit `on_intel` / `on_arm` blocks,
keep `replace: false`; the universal binary then ships through other channels
while the cask serves each Mac its native slice.

## Behavior

- Looks for macOS artifacts (`disk_image` or `archive` kind)
- Requires SHA256 checksum metadata on the artifact
- Emits per-arch `on_intel` / `on_arm` blocks for multi-arch macOS releases; a single macOS arch renders a flat `url` / `sha256`
- Clones the tap repository, writes the cask file, commits, and pushes
- Default commit message: `"Brew cask update for {{ ProjectName }} version {{ Tag }}"`

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

## Artifact eligibility

anodizer folds only genuine macOS targets (`*-apple-darwin`, `darwin-universal`)
into a cask. Apple **non-macOS** targets (`aarch64-apple-ios`, `*-tvos`,
`*-watchos`) are excluded — a cask installs macOS apps, so they never appear in
the generated cask. If the build produces **no** eligible macOS archive,
anodizer fails the release rather than emitting an empty cask. See
[Artifact eligibility](./selecting-publishers.md#artifact-eligibility) for the
full rule.
