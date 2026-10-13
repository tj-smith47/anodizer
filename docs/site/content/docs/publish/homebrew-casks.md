+++
title = "Homebrew Casks"
description = "Generate Homebrew cask formulae for macOS applications"
weight = 85
template = "docs.html"
+++

Anodizer generates Homebrew Cask `.rb` files for your macOS artifacts and pushes them to your tap repository. A cask is the canonical Homebrew channel for **pre-compiled binaries** — both GUI `.app` bundles (via the `app` stanza) and CLI tools (via a `binary` stanza that symlinks an executable onto the user's `PATH`). The [Homebrew Formula](/docs/publish/homebrew/) path — which compiles from source — is deprecated upstream for pre-built release binaries; use a cask. Set `binaries:` to ship a CLI; set `app:` to ship a GUI application; set both when a bundle also exposes a CLI.

Generated casks pass `brew style`: stanzas come in Homebrew's order and grouping, arguments and arrays use Homebrew's alignment, macOS-only stanzas (`app`, `service`) sit inside `on_macos` when the cask also supports Linux, and a cask with no Linux side carries `depends_on :macos` unless its `custom_block` already declares an OS dependency (see [Dependencies](#dependencies-dependencies)). A cask with `hooks` is the one exception; see [Hooks](#hooks).

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
      branch: main                   # optional; push branch (default: repo default branch, or versioned <cask>-<version> when pull_request.enabled)
      pull_request:
        enabled: false               # optional; open PR instead of direct push
        token: ""                    # token used to open the PR (default: repository.token, then GITHUB_TOKEN)
        base: main
    directory: Casks                 # optional; directory in the tap repo
    description: ""                  # optional
    homepage: ""                     # optional
    license: ""                      # optional
    app: ""                          # optional; app stanza name
    binaries: []                     # optional; binaries to symlink
    manpages: []                     # optional
    caveats: ""                      # optional; post-install message
    service: ""                      # optional; Services bundle (.workflow) to install
    custom_block: ""                 # optional; raw Ruby inserted into cask
    depends_on_macos: true           # optional; overrides the custom_block scan
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
| `name` | string | project name | Cask name. The token — the `.rb` file name and the `cask "..."` header — is this value lowercased with spaces as dashes (`Foo Bar` publishes `Casks/foo-bar.rb`); the human-readable `name "..."` stanza keeps it verbatim |
| `repository` | object | **required** | Tap repository (`owner`, `name`) |
| `directory` | string | `Casks` | Directory in the tap repo |
| `description` | string | Cargo `[package].description` | Cask description. Derived from `Cargo.toml`; set to override. |
| `homepage` | string | Cargo `[package].homepage` | Homepage URL. Derived from `Cargo.toml`; set to override. |
| `license` | string | none | License identifier |
| `app` | string | none | Application name for `app` stanza |
| `binaries` | list | none | Binaries to symlink |
| `manpages` | list | none | Man pages to install |
| `caveats` | string | none | Post-install caveats message |
| `service` | string | none | Path of a macOS Services bundle (for example `MyTool.workflow`) inside the download, written as `service "<path>"` |
| `custom_block` | string | none | Raw Ruby inserted into the cask |
| `depends_on_macos` | bool | derived | Whether the cask renders `depends_on :macos`. See [Dependencies](#dependencies-dependencies). |
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
| `verified` | string | none | **Deprecated.** Homebrew [removed the `verified:` stanza](https://github.com/Homebrew/brew/pull/23280) and now applies its default URL verification. Setting it still parses, logs a deprecation warning, and writes nothing into the cask. |
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

### Hooks (`hooks`) {#hooks}

```yaml
hooks:
  pre:
    install: "system_command '/usr/bin/some-setup'"
    uninstall: "system_command '/usr/bin/some-cleanup'"
  post:
    install: "system_command '/usr/bin/post-setup'"
    uninstall: "system_command '/usr/bin/post-cleanup'"
```

Each hook is written as a `preflight do` / `postflight do` /
`uninstall_preflight do` / `uninstall_postflight do` block holding the Ruby
as given. Homebrew 7 deprecates these blocks in favour of `preflight_steps`
and its siblings, which accept only Homebrew's own step calls rather than
arbitrary Ruby, so `brew style` reports `Cask/InstallSteps` for each hook. The
cask still installs; Homebrew prints a deprecation warning when it loads it.

### Generated completions (`generate_completions_from_executable`)

| Field | Type | Description |
|-------|------|-------------|
| `executable` | string | Binary to generate completions from |
| `args` | list | Arguments to pass to the executable |
| `base_name` | string | Base name for completion files |
| `shell_parameter_format` | string | Completion framework type (arg, clap, cobra, etc.) |
| `shells` | list | Target shells (bash, zsh, fish, pwsh) |

### Dependencies (`dependencies`)

Each entry can specify either `cask` or `formula`. The cask lists casks
first, then formulae, each group sorted by name without regard to case
with the names written as given, the order `brew style` requires:

```yaml
dependencies:
  - formula: cmake
  - cask: xquartz
  - cask: Example/tap/app
```

```ruby
  depends_on cask: "Example/tap/app"
  depends_on cask: "xquartz"
  depends_on formula: "cmake"
  depends_on :macos
```

`depends_on :macos` joins the group whenever the cask has no Linux side.
Homebrew installs a cask on Linux unless it depends on macOS, so without the
line a cask of macOS `binaries` would install a Mach-O executable on a Linux
host. A cask has a Linux side when the release ships Linux artifacts (an
`on_linux` block) or its `custom_block` contains the word `on_linux`,
`on_system` or `app_image` anywhere, as a whole word: `self.on_linux do`,
`(on_linux do` and a one-line `on_arm do on_linux do … end end` all count.
The line is also left out when the `custom_block` contains the word
`depends_on` and any of `:macos`, `macos:` (as in `maximum_macos:`),
`:linux` or `linux:`, since a second OS dependency is redundant.

A flat single-download cask whose `custom_block` gives it a Linux side puts
its `sha256` / `url` (and `app` / `service`) inside `on_macos`, so a Linux
host never downloads the macOS archive.

The `custom_block` is scanned as plain text, so a word inside a string, a
comment or a heredoc body counts as well. For a block that writes its
stanzas literally, the scan can only find a Linux side or an OS dependency
that Ruby would not; it misses only a stanza built at runtime. A wrong answer in either
direction gives a cask that works on macOS and that Homebrew refuses on
Linux. A download inside `on_macos` with no real Linux block and no
`depends_on :macos` fails to load with `Missing Linux stanzas can leave Linux
sha256 as nil. Add depends_on :macos`. A `depends_on :macos` beside a real
Linux side the scan could not see (an `on_linux` built at runtime) loads, and
a Linux install stops with `This cask requires macOS.` Set
`depends_on_macos:` when the scan is wrong for your `custom_block`:

```yaml
homebrew_casks:
  - name: mytool
    binaries: [mytool]
    # The scan reads the word on_linux in this comment as a Linux side.
    custom_block: |
      # Linux users install the mytool formula; on_linux is not used here.
    depends_on_macos: true
```

```ruby
# This file was generated by anodizer (https://github.com/tj-smith47/anodizer). DO NOT EDIT.
cask "mytool" do
  # Linux users install the mytool formula; on_linux is not used here.

  version "1.2.3"
  sha256 "2222222222222222222222222222222222222222222222222222222222222222"

  url "https://github.com/myorg/myapp/releases/download/v#{version}/myapp-darwin-arm64.tar.gz"
  name "mytool"

  livecheck do
    skip "Auto-generated on release."
  end

  depends_on :macos

  binary "mytool"
end
```

`true` renders `depends_on :macos` and keeps a flat download at the top
level. `false` renders no OS dependency and places a flat download inside
`on_macos`, for a `custom_block` whose Linux side the scan cannot see. Left
unset, the scan decides. A per-crate cask (`publish.homebrew_cask:` or
`publish.homebrew.cask:`) takes the same field.

### Conflicts (`conflicts`)

Homebrew casks can only conflict with other casks, so set `cask` on each
entry. A `formula` entry is dropped with a deprecation warning, because
`conflicts_with formula:` makes the cask fail to load:

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
host the binary built for its CPU. For this config and a release with
`darwin/amd64` and `darwin/arm64` archives:

```yaml
homebrew_casks:
  - name: myapp
    repository:
      owner: myorg
      name: homebrew-tap
```

the cask reads:

```ruby
# This file was generated by anodizer (https://github.com/tj-smith47/anodizer). DO NOT EDIT.
cask "myapp" do
  version "1.2.3"

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

  name "myapp"

  livecheck do
    skip "Auto-generated on release."
  end

  depends_on :macos

  binary "myapp"
end
```

The version substring in each URL is rewritten to `#{version}` so Homebrew
auto-updates the download on the next release. The `url`/`using` and other
[URL config](#url-config-url) values you set apply inside every per-arch block.

This emission is automatic — there is no flag to enable it. anodizer decides the
shape from the artifacts present in the release:

- **Multiple macOS architectures** → per-arch `on_intel` / `on_arm` blocks (the
  shape above). The same mechanism handles Linux casks: a `darwin/amd64` +
  `darwin/arm64` + `linux/amd64` release produces an `on_macos` block (with two
  arch entries) *and* an `on_linux` block.
- **One macOS architecture** → a flat top-level `url` / `sha256`, with no
  `on_intel` / `on_arm` wrappers:

  ```ruby
  # This file was generated by anodizer (https://github.com/tj-smith47/anodizer). DO NOT EDIT.
  cask "myapp" do
    version "1.2.3"
    sha256 "2222222222222222222222222222222222222222222222222222222222222222"

    url "https://github.com/myorg/myapp/releases/download/v#{version}/myapp-darwin-arm64.tar.gz"
    name "myapp"

    livecheck do
      skip "Auto-generated on release."
    end

    depends_on :macos

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

A lipo'd universal binary has the synthetic `darwin-universal` target
(architecture `all`) and serves every Mac. In the cask it fills whichever of
the `on_arm` / `on_intel` slots no per-arch macOS artifact fills: beside an
`arm64` build alone, `on_arm` downloads the `arm64` build and `on_intel` the
universal one. The universal artifact fills that slot only when it and the
per-arch artifact carry the same build id; when the ids differ or either one
has none, it is left out and `-v` names it as skipped. When both slots are filled the
universal artifact is left out, and `-v` prints:

```text
skipped universal artifact 'myapp-darwin-universal.tar.gz' — both macOS architectures have their own download
```

Which shape the cask takes depends on `universal_binaries.replace`:

- **`replace: false`** (or unset) → the per-arch `amd64` and `arm64` artifacts
  stay in the catalog and the universal one is marked as not replacing them,
  so the cask leaves it out and renders the `on_intel` + `on_arm` body above.
- **`replace: true`** → the per-arch macOS artifacts are removed once the
  universal binary is built. A macOS-only release then gets the flat
  top-level `url` / `sha256`, pointing at the universal binary. A release that
  also ships Linux artifacts puts the universal binary's `sha256` / `url`
  directly inside `on_macos`, beside the per-arch `on_linux` blocks. For the
  config above and a `darwin-universal` + `linux/amd64` + `linux/arm64`
  release:

  ```ruby
  # This file was generated by anodizer (https://github.com/tj-smith47/anodizer). DO NOT EDIT.
  cask "myapp" do
    version "1.2.3"

    on_macos do
      sha256 "3333333333333333333333333333333333333333333333333333333333333333"

      url "https://github.com/myorg/myapp/releases/download/v#{version}/myapp-darwin-universal.tar.gz"
    end
    on_linux do
      on_arm do
        sha256 "5555555555555555555555555555555555555555555555555555555555555555"
        url "https://github.com/myorg/myapp/releases/download/v#{version}/myapp-linux-arm64.tar.gz"
      end
      on_intel do
        sha256 "4444444444444444444444444444444444444444444444444444444444444444"
        url "https://github.com/myorg/myapp/releases/download/v#{version}/myapp-linux-amd64.tar.gz"
      end
    end

    name "myapp"

    livecheck do
      skip "Auto-generated on release."
    end

    binary "myapp"
  end
  ```

So if you want a multi-arch cask with explicit `on_intel` / `on_arm` blocks,
keep `replace: false`; the universal binary then ships through other channels
while the cask serves each Mac its native slice.

## Behavior

- Looks for macOS artifacts (`disk_image` or `archive` kind)
- Requires SHA256 checksum metadata on the artifact
- Emits per-arch `on_intel` / `on_arm` blocks for multi-arch macOS releases, with a universal binary filling a slot no per-arch build covers; a single macOS arch, or a universal binary alone, renders a flat `url` / `sha256` (inside `on_macos` when the cask also has a Linux side)
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
