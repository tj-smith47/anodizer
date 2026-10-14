+++
title = "Homebrew"
description = "Generate Homebrew formulae and push to tap repositories"
weight = 3
template = "docs.html"
+++

> **Deprecation:** `publish.homebrew` (Homebrew **Formula**) is deprecated upstream
> in [GoReleaser v2.16](https://goreleaser.com/blog/goreleaser-v2.16/) and anodizer
> follows the same deprecation. The replacement is **[Homebrew Casks](./homebrew-casks.md)** —
> the canonical Homebrew distribution channel for pre-compiled binaries.
>
> Quoting upstream: _"Migrate to `homebrew_casks`, which is the right tool for the
> job: it's how Homebrew expects pre-compiled binaries to be distributed, and it
> gets all the new features (completion generation, post-install hooks, and so on)."_
>
> Anodizer still parses `publish.homebrew` for back-compat; a `DEPRECATION:` warning
> is emitted at config-load time. New projects should write `homebrew_casks:`
> (top-level) or `publish.homebrew_cask:` (per-crate) directly. See the
> [GoReleaser migration guide](../../migration/goreleaser.md#brews-homebrew_casks)
> for the side-by-side YAML diff.

Anodizer generates Ruby Homebrew formulae with multi-platform support and pushes them to your tap repository.

> **Tap vs. homebrew-core:** this publisher pushes a formula to **your own tap**
> (`youruser/homebrew-tap`). To bump a formula that has already been accepted into
> `Homebrew/homebrew-core`, use the [Homebrew Core](./homebrew-core.md) publisher
> (`homebrew_cores`), which opens a fork-based pull request against the core repo;
> this publisher pushes to a tap you control.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | false | re-clone tap, `git revert HEAD --no-edit`, push | `GITHUB_TOKEN contents:write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## The `required:` field

Default: **`false`** — a Homebrew push failure is logged but does not fail the release.

Set `required: true` to make the release exit non-zero if this publisher fails:

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        repository:
          owner: myorg
          name: homebrew-tap
        required: true
```

See [Publish overview — the `required:` field](../) for the full semantics.

## Minimal config

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        repository:
          owner: myorg
          name: homebrew-tap
```

## Homebrew config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `repository.owner` | string | — | GitHub owner of the tap repo |
| `repository.name` | string | — | Tap repository name |
| `directory` | string | `Formula` | Subdirectory within the tap repo for the `.rb` file |
| `description` | string | Cargo `[package].description` | Formula description. Derived from `Cargo.toml`; set to override. |
| `license` | string | Cargo `[package].license` | License identifier. Derived from `Cargo.toml`; set to override. |
| `install` | string | auto | Custom install block (Ruby) |
| `test` | string | none | Custom test block (Ruby) |
| `dependencies` | list | none | Formula dependencies (`name`, `type`, `version`, `os`). See [Dependencies](#dependencies). |
| `conflicts` | list | none | `conflicts_with` entries (`name`, `because`). |
| `livecheck` | object | skip | `livecheck do … end` stanza — version polling. See [Livecheck](#livecheck). |
| `manpages` | list | none | Man-page file paths to install via `manN.install`. See [Auto completions and man pages](#auto-completions-and-man-pages). |
| `completions` | object | none | Pre-built completion file paths (`bash`/`zsh`/`fish`). |
| `generate_completions_from_executable` | object | none | Generate completions by running the installed binary at install time. |

## Full config reference

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        repository:
          owner: myorg          # required
          name: homebrew-tap    # required
          token: ""             # default: HOMEBREW_TAP_TOKEN, then the GitHub token chain
          branch: ""            # default: repo default branch (versioned <formula>-<version> when pull_request.enabled)
          pull_request:
            enabled: false
            draft: false
            token: ""           # token used to open the PR (default: repository.token, then ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN / GH_TOKEN)
            base:
              owner: ""
              name: ""
              branch: ""
        directory: Formula      # subdirectory in the tap
        description: ""
        license: ""
        install: ""             # custom Ruby install block
        test: ""                # custom Ruby test block
        dependencies:           # see Dependencies
          - name: openssl@3
          - name: pkg-config
            type: build         # build, test, recommended or optional
          - name: git
            version: ">= 2.40"  # ignored when type is set
            os: mac             # mac or linux
        livecheck:              # version polling; default skips (see Livecheck)
          strategy: github_latest
          url: stable
        generate_completions_from_executable:
          executable: myapp
          args: [completion]
          shells: [bash, zsh, fish]
        manpages:
          - myapp.1
        skip_upload: false      # bool or "auto" (skip prereleases)
        cask:                   # per-crate cask config (same shape as homebrew_casks[])
          update_existing_pr: false
```

## Homebrew Cask config fields

A per-crate cask is configured under `publish.homebrew_cask:`, or under the
`cask:` sub-block of `publish.homebrew:`. The two spellings are the same
setting and publish identically; set one of them per crate (a block that sets
both is a config error). Top-level casks go in `homebrew_casks:`. All three use
the same `HomebrewCaskConfig` shape.

Where a per-crate cask is pushed:

| Config | Tap | Commit and PR |
|---|---|---|
| a `homebrew:` formula and a cask with no `repository:` of its own (or the formula's) | the formula's tap | one commit and one PR for the formula and the cask |
| the same, with the formula's `skip_upload:` or `if:` leaving it out of the run | the formula's tap | the cask's own commit and PR |
| a `homebrew:` formula and a cask whose `repository:` names another tap | the cask's `repository:` | its own commit and PR |
| a `homebrew:` formula and a cask whose `repository:` names the formula's tap with a different `branch:`, `token:` or `pull_request:` | the cask's `repository:` | its own commit and PR |
| a cask and no `homebrew:` formula | the cask's `repository:` | its own commit and PR |

A cask's `repository:` names the formula's tap when its `owner` and `name`
match the formula's without regard to case (`MyOrg/Homebrew-Tap` is
`myorg/homebrew-tap`).

The formula and the cask are gated separately. A cask's own `skip_upload:`
and `if:` win; one it leaves unset (or an empty `if: ""`) inherits the
formula's:

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        skip_upload: true       # the formula is not pushed
        repository:
          owner: myorg
          name: homebrew-tap
      homebrew_cask:
        skip_upload: false      # the cask is, alone, to homebrew-tap
        binaries: [myapp]
```

```yaml
crates:
  - name: myapp
    publish:
      homebrew_cask:            # no formula: the cask is pushed on its own
        repository:
          owner: myorg
          name: homebrew-tap
        binaries: [myapp]
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | crate name | Cask name |
| `repository` | object | — | Tap repo (`owner`, `name`, `token`, `branch`, `pull_request`) |
| `directory` | string | `Casks` | Subdirectory in the tap for the `.rb` file |
| `app` | string | none | macOS `.app` bundle name |
| `binaries` | list | none | Binary stubs to install |
| `description` | string | Cargo `[package].description` | Cask description. Derived from `Cargo.toml`; set to override. |
| `homepage` | string | Cargo `[package].homepage` | Project homepage. Derived from `Cargo.toml`; set to override. |
| `skip_upload` | bool or string | the formula's `skip_upload`, else `false` | Skip publishing; `true` or `1` always skips, `auto` skips for prereleases, `false`, `0` or empty publishes; a template is rendered and read the same way, in any letter case (see [bool-or-template values](@/docs/general/templates.md#bool-or-template-values)) |
| `update_existing_pr` | bool or string | `false` | Force-push to an existing open PR branch. When `false`, an existing PR is skipped. See [Cask existing PR behavior](#cask-existing-pr-behavior). |

`defaults.publish.homebrew_cask:` merges into each crate's cask field by
field: a field the crate's cask sets wins, a field it leaves unset comes
from the defaults (nested blocks such as `repository:` merge the same way),
and a crate with no cask of its own gets the defaults' cask whole.

```yaml
defaults:
  publish:
    homebrew_cask:
      repository:
        owner: myorg
        name: homebrew-tap
      homepage: https://myorg.example

crates:
  - name: myapp
    publish:
      homebrew_cask:
        binaries: [myapp]
        homepage: https://myapp.example   # wins over the defaults
        # repository: myorg/homebrew-tap, from the defaults
```

## Skipped formulae and casks

A gate that leaves a formula or a cask out of the run prints one line for
it, once per run:

| Gate | Line |
|---|---|
| formula `skip_upload:` | `skipped homebrew for 'myapp' upload — skip_upload is set` |
| formula `if:` | ``skipped homebrew for 'myapp' — `if` condition evaluated falsy`` |
| cask `skip_upload:` (its own, else the formula's) | `skipped homebrew cask for 'myapp' upload — skip_upload is set` |
| cask `if:` (its own, else the formula's) | ``skipped homebrew cask for 'myapp' — `if` condition evaluated falsy`` |

When only the formula's gate trips, a cask that shares its tap is still
pushed, in a commit and pull request of its own (see the table above).

The pull request token (`repository.pull_request.token`, else
`repository.token`) is rendered before the tap is cloned. A template that
does not render, or a block with no `repository:`, skips that crate with
nothing pushed; the crates after it still run, and the run summary names the
reason (`pull_request.token does not render: <cause>`, or
`repository.name is not set`). The cause never repeats the template's text,
so a secret written beside a template stays out of the log. A per-crate
cask records its skip under `homebrew-cask`, and a top-level `homebrew_casks:`
entry under `homebrew-cask` with the cask's name. The per-entry line
`skipped homebrew for 'myapp' — <reason>` prints under `--show-skipped`:

```bash
anodizer release --show-skipped
```

In a GitHub Actions job summary, a tap push reads `Updated homebrew <name>
in <tap URL>`, or `Opened pull request to <owner/repo> (homebrew <name>):
<url>` in pull request mode, for a formula and a cask alike. See
[Job summary](../ci/github-actions.md#job-summary).

## Authentication

The push to your tap repository uses the first token that is set:

1. `repository.token`
2. `HOMEBREW_TAP_TOKEN`
3. the run's GitHub token (`--token`, or the `ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN` / `GH_TOKEN` value read at startup); a GitLab or Gitea token is never sent to GitHub
4. `ANODIZER_GITHUB_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN`

The pull request is opened with `repository.pull_request.token`, then `repository.token`, then `ANODIZER_GITHUB_TOKEN`, `GITHUB_TOKEN` and `GH_TOKEN`. With none of these set, the `gh` CLI's own login opens it; with neither a token nor `gh`, the publish fails. The token needs push access to the tap, and `pull_request:write` when `pull_request.enabled` is set.

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        repository:
          owner: myorg
          name: homebrew-tap
          token: "{{ .Env.HOMEBREW_TAP_TOKEN }}"
          pull_request:
            enabled: true
            token: "{{ .Env.PR_TOKEN }}"
```

## Common gotchas

- **Branch protection**: if your tap repo has branch protection enabled, direct push will fail. Use a fork + PR workflow via `repository.pull_request`.
- **Multiple platforms**: anodizer auto-generates `on_macos` / `on_linux` / `on_intel` / `on_arm` stanzas from the build targets. If your build only produces one platform, the formula contains a single `url` block with no platform stanzas.
- **Cask vs formula**: formulae install from archives; casks install macOS `.app` bundles. A crate can have both if `publish.homebrew` (formula) and `publish.homebrew_cask` (or `publish.homebrew.cask`, or a top-level `homebrew_casks:` entry) are both configured.

## Republish / update behavior

Formula files are updated in-place on each release; re-cutting the same version overwrites the formula in the tap (prior commit stays in git history). The Manager group rollback reverts via `git revert HEAD --no-edit` + push.

Casks open a PR per version. Set `update_existing_pr: true` on the cask config to force-push to an existing open PR, so no duplicate is opened — full detail in the next section.

## Cask existing PR behavior

When `gh pr create` reports a PR for the same head branch already exists,
Anodizer's default is to **skip and emit a warning**:

```
homebrew cask: PR for 'owner:myapp-cask-1.2.3' already exists — skipping
               (set update_existing_pr: true to update the PR in place)
```

Setting `update_existing_pr: true` force-pushes the updated cask file to the
existing branch using `--force-with-lease`, so the open PR picks up the new
content without creating a duplicate:

```yaml
# per-crate cask (either spelling)
crates:
  - name: myapp
    publish:
      homebrew_cask:
        update_existing_pr: true

# top-level homebrew_casks array
homebrew_casks:
  - name: myapp
    update_existing_pr: true
```

## Livecheck

By default the formula emits `livecheck do skip "Auto-generated on release." end`
— a binary tap's archive URL and SHA are rewritten on every release, so there is
nothing stable for `brew livecheck` to poll. Set a `strategy` (and optionally
`url`/`regex`) to opt into active version detection instead. anodizer tags
releases on GitHub, so `github_latest` against `:stable` is the right pairing:

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        livecheck:
          strategy: github_latest
          url: stable
```

renders into the formula:

```ruby
  livecheck do
    url :stable
    strategy :github_latest
  end
```

`url` accepts a Ruby symbol shorthand (`stable` / `head` / `homepage` →
`url :stable`) or a literal URL string. `regex` is raw Ruby (e.g.
`%r{v(\d+\.\d+)}i`) for `page_match`-style strategies. Setting `skip: false`
without any of `strategy`/`url`/`regex` falls back to `skip` with a warning —
an empty `livecheck do … end` is invalid.

## Auto completions and man pages

The formula installs shell completions and man pages without raw Ruby in the
install block:

- **`generate_completions_from_executable`** renders the modern homebrew-core
  idiom `generate_completions_from_executable(bin/"<exe>", ...)`, calling the
  installed binary at install time to emit its own completions. Preferred for a
  CLI that can print its completions (ripgrep/fd/bat all use this form).
- **`manpages`** renders one `man1.install "<path>"` line per entry (a path
  ending in `.N` for N in 1–8 routes to the matching `manN` section).
- **`completions`** installs pre-built completion files when the archive ships
  ready-made ones (`bash_completion.install` / `zsh_completion.install` /
  `fish_completion.install`).

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        generate_completions_from_executable:
          executable: myapp
          args: [completion]
          shells: [bash, zsh, fish]
        manpages:
          - myapp.1
```

## Dependencies

Each `dependencies` entry renders one `depends_on` line. `type` renders as a
Ruby symbol and takes precedence over `version`; `os` moves the line into the
formula's one `on_macos do` or `on_linux do` block, the same block that carries the per-OS `url` and `sha256` lines when the archives are split by OS. The formula lists them in the order
`brew style` requires: by type (`build`, `test`, runtime, `recommended`,
`optional`), then by name without regard to case, the names written as given.

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        dependencies:
          - name: Azure/kubelogin/kubelogin
          - name: aws-iam-authenticator
          - name: pkg-config
            type: build
          - name: fzf
            type: optional
```

```ruby
  depends_on "pkg-config" => :build
  depends_on "aws-iam-authenticator"
  depends_on "Azure/kubelogin/kubelogin"
  depends_on "fzf" => :optional
```

## The `test do` block

Every generated formula carries a `test do` block (`brew test` runs it). When
`test:` is unset, anodizer emits a sensible default that invokes the installed
binary; set `test:` to supply your own Ruby assertions:

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        test: |
          assert_match "myapp #{version}", shell_output("#{bin}/myapp --version")
```

## Dual-license rendering

When the resolved license is a compound SPDX expression (`MIT OR Apache-2.0`),
anodizer renders Homebrew's `any_of` license form (`license any_of: [...]`), so the formula declares both licenses the way Homebrew audits
expect. A single-license crate renders the plain `license "<spdx>"` line.

## Generated formula

Anodizer generates a formula with:
- Multi-platform download URLs (`on_macos`, `on_linux`, `on_intel`, `on_arm`)
- SHA-256 checksums for each archive
- Automatic binary installation
- Package name normalization (underscores → hyphens)

The stanzas follow Homebrew's formula component order, the order
`brew style` checks: `desc`, `homepage`, `url`, `version`, `sha256`,
`license`, `livecheck`, `depends_on`, `on_macos`, `on_linux`,
`conflicts_with`, `custom_block`, `def install`, `def post_install`,
`def caveats`, `plist`, `service`, `test`. One top-level `def install`
serves every platform.

Homebrew loads a formula on macOS and Linux, each on arm64 and x86_64
(`brew readall` checks all four), and each of them needs a `url`. The
downloads are placed like this:

| Archives released | Formula |
|---|---|
| macOS and Linux | `on_macos` and `on_linux` blocks, each with an `on_arm` and an `on_intel` block |
| one OS | a top-level `url`, plus `depends_on :macos` or `depends_on :linux`; an Intel download that differs goes in an `on_intel` block inside that OS's block |
| one CPU for an OS (arm64 macOS, or one Linux CPU) | that archive serves both CPUs of the OS, with `depends_on arch: :arm64` or `arch: :x86_64`, so `brew install` refuses the other CPU with Homebrew's own message |
| Intel-only macOS | the Intel build serves Apple Silicon too, which runs it under Rosetta, and `def caveats` adds a note shown there |
| universal macOS | the universal build serves both CPUs |

Homebrew on Linux runs on x86_64 and arm64 only. A 32-bit archive (`i686`,
`armv6`, `armv7`), any other CPU, and an OS other than macOS and Linux are
left out of the formula, each with a `-v` line:

```text
skipped archive 'myapp-linux-armv7.tar.gz' — Homebrew on Linux has no armv7 variant
```

`caveats`, `custom_block`, `plist` and `service` are free-form text. Their
shared indentation is removed and each line is indented for its place in
the formula, so a flush-left YAML block scalar and a pre-indented one render
the same. `custom_block` and `service` are read as Ruby code, and only
their lines that begin in code move: a line inside a multi-line string,
percent literal, regex, heredoc body or `=begin` comment, and the closing
tag of a `<<` or `<<-` heredoc, is kept byte for byte, and trailing
whitespace is dropped only from a line that ends in code. The
[cask page](./homebrew-casks.md#custom-block) describes
how anodizer reads that Ruby. Two stanzas raise a `brew style` offence that comes from the
config itself: `post_install` renders as `def post_install`, which Homebrew
now asks to be written as `post_install_steps`, and `plist` renders the
deprecated `def plist`, which `service` replaces. Homebrew 7 removed the
`plist_options` stanza that went with `def plist`, so the formula carries
`def plist` alone, and the render logs a deprecation warning pointing to
`service`.

## Full example

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        repository:
          owner: myorg
          name: homebrew-tap
        directory: Formula
        description: "Fast CLI tool"
        license: MIT
        install: |
          bin.install "myapp"
        test: |
          system "#{bin}/myapp", "--version"
        livecheck:
          strategy: github_latest
          url: stable
```

For a release of `myapp` with an Apple Silicon archive and a Linux x86_64
archive, it renders:

```ruby
# This file was generated by anodizer (https://github.com/tj-smith47/anodizer). DO NOT EDIT.
# typed: false
# frozen_string_literal: true

class Myapp < Formula
  desc "Fast CLI tool"
  homepage "https://github.com/myorg/myapp"
  version "1.2.3"
  license "MIT"

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    depends_on arch: :arm64

    on_arm do
      url "https://github.com/myorg/myapp/releases/download/v1.2.3/myapp-darwin-arm64.tar.gz"
      sha256 "1111111111111111111111111111111111111111111111111111111111111111"
    end
    on_intel do
      url "https://github.com/myorg/myapp/releases/download/v1.2.3/myapp-darwin-arm64.tar.gz"
      sha256 "1111111111111111111111111111111111111111111111111111111111111111"
    end
  end

  on_linux do
    depends_on arch: :x86_64

    on_arm do
      url "https://github.com/myorg/myapp/releases/download/v1.2.3/myapp-linux-amd64.tar.gz"
      sha256 "2222222222222222222222222222222222222222222222222222222222222222"
    end
    on_intel do
      url "https://github.com/myorg/myapp/releases/download/v1.2.3/myapp-linux-amd64.tar.gz"
      sha256 "2222222222222222222222222222222222222222222222222222222222222222"
    end
  end

  def install
    bin.install "myapp"
  end

  test do
    system "#{bin}/myapp", "--version"
  end
end
```

## Artifact eligibility

**Formula:** anodizer folds only the archives Homebrew can install into the
formula: genuine macOS targets (`*-apple-darwin`, `darwin-universal`) plus the
Linux archives it supports. Apple **non-macOS** targets (`aarch64-apple-ios`,
`*-tvos`, `*-watchos`) are excluded — they never appear in the generated
formula. If the build produces **no** eligible archive, anodizer fails the
release with an error that names every archive it looked at, each with its
OS and CPU (`homebrew formula 'myapp': none of its archives is for a
platform Homebrew installs on (macOS, or Linux on x86_64 or arm64):
myapp-freebsd-amd64.tar.gz (freebsd/amd64)`). See
[Artifact eligibility](./selecting-publishers.md#artifact-eligibility) for how
this compares to the other install aggregators.

**Cask:** the per-crate cask (`homebrew_cask:` / `cask:`) is
**macOS-only** — anodizer generates it only when the build produces a genuine
macOS artifact (`*-apple-darwin`); a Linux archive alone produces no cask (Linux
archives of the same crate join it as an `on_linux` block), matching the
standalone [Homebrew Casks](./homebrew-casks.md#artifact-eligibility) publisher.
