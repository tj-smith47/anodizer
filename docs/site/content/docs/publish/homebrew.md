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
          token: ""             # falls back to GITHUB_TOKEN
          branch: ""            # default: repo default branch
          pull_request:
            enabled: false
            draft: false
            base:
              owner: ""
              name: ""
              branch: ""
        directory: Formula      # subdirectory in the tap
        description: ""
        license: ""
        install: ""             # custom Ruby install block
        test: ""                # custom Ruby test block
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

Casks are configured under `publish.homebrew.cask:` (per-crate) or `homebrew_casks:` (top-level array). Both axes use the same `HomebrewCaskConfig` shape.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | crate name | Cask name |
| `repository` | object | — | Tap repo (`owner`, `name`, `token`, `branch`, `pull_request`) |
| `directory` | string | `Casks` | Subdirectory in the tap for the `.rb` file |
| `app` | string | none | macOS `.app` bundle name |
| `binaries` | list | none | Binary stubs to install |
| `description` | string | Cargo `[package].description` | Cask description. Derived from `Cargo.toml`; set to override. |
| `homepage` | string | Cargo `[package].homepage` | Project homepage. Derived from `Cargo.toml`; set to override. |
| `skip_upload` | bool or string | `false` | Skip publishing; `true` always skips, `"auto"` skips for prereleases |
| `update_existing_pr` | bool or string | `false` | Force-push to an existing open PR branch instead of skipping. See [Cask existing PR behavior](#cask-existing-pr-behavior). |

## Authentication

| Variable | Description |
|----------|-------------|
| `GITHUB_TOKEN` | Token with push access to your tap repository (and `pull_request:write` for cask PR mode) |

The token can also be set via `repository.token` in the config.

## Common gotchas

- **Branch protection**: if your tap repo has branch protection enabled, direct push will fail. Use a fork + PR workflow via `repository.pull_request`.
- **Multiple platforms**: anodizer auto-generates `on_macos` / `on_linux` / `on_intel` / `on_arm` stanzas from the build targets. If your build only produces one platform, the formula contains a single `url` block instead of the multi-platform form.
- **Cask vs formula**: formulae install from archives; casks install macOS `.app` bundles. A crate can have both if `publish.homebrew` (formula) and `publish.homebrew.cask` (or top-level `homebrew_casks:`) are both configured.

## Republish / update behavior

Formula files are updated in-place on each release; re-cutting the same version overwrites the formula in the tap (prior commit stays in git history). The Manager group rollback reverts via `git revert HEAD --no-edit` + push.

Casks open a PR per version. Set `update_existing_pr: true` on the cask config to force-push to an existing open PR rather than opening a duplicate — full detail in the next section.

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
# per-crate cask
publish:
  homebrew:
    cask:
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
homebrew:
  generate_completions_from_executable:
    executable: myapp
    args: [completion]
    shells: [bash, zsh, fish]
  manpages:
    - myapp.1
```

## The `test do` block

Every generated formula carries a `test do` block (`brew test` runs it). When
`test:` is unset, anodizer emits a sensible default that invokes the installed
binary; set `test:` to supply your own Ruby assertions:

```yaml
homebrew:
  test: |
    assert_match "myapp #{version}", shell_output("#{bin}/myapp --version")
```

## Dual-license rendering

When the resolved license is a compound SPDX expression (`MIT OR Apache-2.0`),
anodizer renders Homebrew's `any_of` license form rather than a single
`license "MIT"` — so the formula declares both licenses the way Homebrew audits
expect. A single-license crate renders the plain `license "<spdx>"` line.

## Generated formula

Anodizer generates a formula with:
- Multi-platform download URLs (`on_macos`, `on_linux`, `on_intel`, `on_arm`)
- SHA-256 checksums for each archive
- Automatic binary installation
- Package name normalization (underscores → hyphens)

## Full example

```yaml
publish:
  homebrew:
    repository:
      owner: myorg
      name: homebrew-tap
    directory: Formula
    description: "A fast CLI tool"
    license: MIT
    install: |
      bin.install "myapp"
    test: |
      system "#{bin}/myapp", "--version"
    livecheck:
      strategy: github_latest
      url: stable
```
