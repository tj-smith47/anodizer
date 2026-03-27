# Anodize Documentation Site — Design Spec

**Date:** 2026-03-27
**Status:** Approved
**Scope:** Zola documentation site + xtask doc generator

---

## Overview

A Zola-powered documentation site for anodize that closely mirrors GoReleaser's site structure and navigation. Custom theme with a copper/rust visual identity. A `crates/xtask/` crate auto-generates CLI and configuration reference pages from the actual Rust types. Deployed to GitHub Pages.

## Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Static site generator | Zola | Hugo equivalent written in Rust. Same mental model (content dir, TOML config, markdown + frontmatter). Uses Tera — same template engine anodize ships. |
| Theme | Custom (from scratch) | No existing Zola theme matches a GoReleaser-style marketing landing page + sidebar docs. The ~5 templates needed are simple to build; fighting an existing theme would be worse. |
| Landing page layout | Centered hero + terminal demo + feature grid (option C) | Shows the 3-command workflow front and center, followed by feature cards. Can be swapped to split hero (option B) later — the difference is just centering vs flex split. |
| Doc generation | `crates/xtask/` Rust crate | Imports clap command tree and config structs directly. More robust than parsing CLI help output with shell scripts. Uses Tera for rendering. Idiomatic Rust ecosystem pattern (`cargo xtask`). |
| Generation scope | CLI reference + config reference only | These are the two pages that must stay in sync with code (tables of flags, fields, types, defaults). All other content is narrative, better written by hand. |
| Color palette | Copper/rust on dark (CSS variables) | Leans into the "anodize" metallurgy metaphor. Trivially swappable via CSS custom properties. |
| Deployment | GitHub Pages via `gh-pages` branch | GitHub Actions workflow: install Zola, build, deploy on push to main. |

## Directory Structure

```
docs/site/                        # Zola project root
├── config.toml                   # Zola configuration
├── content/
│   ├── _index.md                 # Landing page (uses index.html template)
│   ├── docs/
│   │   ├── _index.md             # Docs root (redirects to getting-started)
│   │   ├── getting-started/
│   │   │   ├── _index.md         # Section index
│   │   │   ├── introduction.md
│   │   │   ├── install.md
│   │   │   ├── quick-start.md
│   │   │   └── how-it-works.md
│   │   ├── general/
│   │   │   ├── _index.md
│   │   │   ├── project-name.md
│   │   │   ├── templates.md
│   │   │   ├── environment.md
│   │   │   └── hooks.md
│   │   ├── builds/
│   │   │   ├── _index.md
│   │   │   ├── rust.md
│   │   │   ├── cross-compilation.md
│   │   │   ├── universal-binaries.md   # skeleton — not yet implemented
│   │   │   └── upx.md                  # skeleton — not yet implemented
│   │   ├── package/
│   │   │   ├── _index.md
│   │   │   ├── archives.md
│   │   │   ├── checksums.md
│   │   │   ├── nfpm.md
│   │   │   ├── docker.md
│   │   │   └── source-sbom.md          # skeleton — not yet implemented
│   │   ├── sign/
│   │   │   ├── _index.md
│   │   │   ├── binaries-archives.md
│   │   │   └── docker.md
│   │   ├── publish/
│   │   │   ├── _index.md
│   │   │   ├── github.md
│   │   │   ├── crates-io.md
│   │   │   ├── homebrew.md
│   │   │   ├── scoop.md
│   │   │   ├── snapshots.md
│   │   │   ├── nightlies.md
│   │   │   ├── custom-publishers.md
│   │   │   ├── chocolatey.md           # skeleton — not yet implemented
│   │   │   ├── winget.md               # skeleton — not yet implemented
│   │   │   ├── aur.md                  # skeleton — not yet implemented
│   │   │   └── krew.md                 # skeleton — not yet implemented
│   │   ├── announce/
│   │   │   ├── _index.md
│   │   │   ├── discord.md
│   │   │   ├── slack.md
│   │   │   └── webhooks.md
│   │   ├── changelog.md
│   │   ├── ci/
│   │   │   ├── _index.md
│   │   │   ├── github-actions.md
│   │   │   └── gitlab-ci.md
│   │   ├── advanced/
│   │   │   ├── _index.md
│   │   │   ├── auto-tagging.md
│   │   │   ├── monorepo.md
│   │   │   ├── config-includes.md      # skeleton — not yet implemented
│   │   │   ├── nightly-builds.md
│   │   │   └── reproducible-builds.md  # skeleton — not yet implemented
│   │   ├── cli.md                      # AUTO-GENERATED by xtask
│   │   └── configuration.md            # AUTO-GENERATED by xtask
│   ├── migration/
│   │   ├── _index.md
│   │   ├── goreleaser.md
│   │   └── cargo-dist.md
│   └── blog/
│       └── _index.md                   # Future: release announcements
├── static/
│   └── favicon.ico
├── templates/
│   ├── base.html                       # Shared: <head>, nav bar, footer
│   ├── index.html                      # Landing page (hero + terminal + grid)
│   ├── docs.html                       # Sidebar + markdown content
│   └── section.html                    # Section index pages
└── sass/
    └── style.scss                      # All styles with CSS custom properties
```

## Navigation Sidebar (mirrors GoReleaser)

The sidebar follows GoReleaser's stage-based organization: configure → build → package → sign → publish → announce. Each section maps to a pipeline stage.

```
Getting Started
  ├── Introduction
  ├── Install
  ├── Quick Start
  └── How It Works

Documentation
  ├── General
  │   ├── Project Name
  │   ├── Templates
  │   ├── Environment Variables
  │   └── Global Hooks
  ├── Build
  │   ├── Rust Builds
  │   ├── Cross-Compilation
  │   ├── Universal Binaries          ← coming soon
  │   └── UPX Compression             ← coming soon
  ├── Package & Archive
  │   ├── Archives
  │   ├── Checksums
  │   ├── nFPM (deb/rpm/apk)
  │   ├── Docker
  │   └── Source Archives + SBOM      ← coming soon
  ├── Sign
  │   ├── Binaries & Archives
  │   └── Docker Images
  ├── Publish
  │   ├── GitHub Releases
  │   ├── crates.io
  │   ├── Homebrew
  │   ├── Scoop
  │   ├── Chocolatey                  ← coming soon
  │   ├── Winget                      ← coming soon
  │   ├── AUR                         ← coming soon
  │   ├── Krew                        ← coming soon
  │   ├── Snapshots
  │   ├── Nightlies
  │   └── Custom Publishers
  ├── Announce
  │   ├── Discord
  │   ├── Slack
  │   └── Webhooks
  ├── Changelog
  ├── CI/CD Integration
  │   ├── GitHub Actions
  │   └── GitLab CI
  └── Advanced
      ├── Auto-Tagging
      ├── Monorepo Support
      ├── Config Includes              ← coming soon
      ├── Nightly Builds
      └── Reproducible Builds          ← coming soon

CLI Reference                          ← auto-generated
Configuration Reference                ← auto-generated

Migration
  ├── From GoReleaser
  └── From cargo-dist
```

## Landing Page

Layout: **Centered hero + terminal demo + feature grid**

### Hero Section
- Title: "Release engineering for Rust, simplified."
- Subtitle: "The declarative release pipeline GoReleaser users wish existed for Rust."
- CTAs: "Get Started" (primary, links to /docs/getting-started/quick-start/) and "GitHub" (secondary, links to repo)

### Terminal Demo
Immediately below the hero, a styled code block showing:
```
$ cargo install anodize
$ anodize init          # generates .anodize.yaml from Cargo.toml
$ anodize release       # build → archive → checksum → release → publish
```

### Feature Grid
6 cards in a 3×2 grid:

| Card | Description |
|------|-------------|
| Full Pipeline | Build → Archive → Checksum → Changelog → Release → Publish → Announce |
| Cargo-Native | Workspace-aware, cross-compilation, cargo-binstall metadata |
| Familiar Config | Same YAML structure GoReleaser users already know |
| Tera Templates | Conditionals, pipes, filters — not regex substitution |
| Package Managers | Homebrew, Scoop, crates.io, nFPM, Docker |
| Single Binary | `cargo install`, no runtime deps, fast startup |

## Documentation Pages

Each doc page uses the `docs.html` template:

- **Left sidebar** (240px): collapsible section navigation, current page highlighted
- **Content area**: rendered markdown with syntax-highlighted code blocks
- **No right-side TOC** initially (can add later via Zola's `toc` variable)

### Page structure convention

Every doc page follows this pattern (matching GoReleaser):
1. Title and one-line description
2. Minimal config example showing the feature
3. Explanation of each config field
4. Full config example with all options
5. Notes/tips where relevant

### Content strategy

- **Implemented features**: Full prose with config examples, CLI usage, explanations. Content drawn from existing `docs/configuration.md`, `docs/templates.md`, design spec, and source code doc comments.
- **Planned features**: Skeleton pages with a "Coming Soon" callout and a brief description of what the feature will do. Consistent format so they're easy to fill in later.

## xtask: `cargo xtask gen-docs`

### Crate setup

```toml
# crates/xtask/Cargo.toml
[package]
name = "xtask"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
anodize = { path = "../cli" }       # package name is "anodize"; lib target is "anodize_cli"
anodize-core = { path = "../core" } # for config types
clap = { version = "4", features = ["derive"] }
tera.workspace = true               # must add tera to [workspace.dependencies] in root Cargo.toml
```

`.cargo/config.toml` gets the conventional xtask alias:
```toml
[alias]
xtask = "run --package xtask --"
```

The CLI crate needs to expose its clap `Command` builder as a public function in a library target (e.g., `pub fn build_cli() -> clap::Command`) so xtask can import it without depending on the binary.

### CLI reference generation

1. Import the root `clap::Command` from the CLI crate
2. Walk the command tree recursively
3. For each command: name, about, long_about, all args (name, short, long, help, default, required, value_names)
4. Render into `docs/site/content/docs/cli.md` using a Tera template
5. Output format: one section per command, table of flags per command

### Configuration reference generation

1. Use `schemars` or manual introspection of config types to extract field names, types, defaults, and doc comments
2. Render into `docs/site/content/docs/configuration.md` using a Tera template
3. Output format: one section per config block (builds, archives, checksum, release, publish, etc.), table of fields per block

Both generated files include a header:
```markdown
+++
title = "CLI Reference"
# AUTO-GENERATED by `cargo xtask gen-docs` — do not edit manually
+++
```

### Running

```bash
cargo xtask gen-docs              # regenerates both files
cargo xtask gen-docs --check      # exits non-zero if files are stale (for CI)
```

## Color Palette

Defined as CSS custom properties in `sass/style.scss`, trivially swappable:

```scss
:root {
  --color-primary: #e8590c;                        // Rust orange — accent, links, active nav
  --color-primary-hover: #c44200;                  // Darker orange for hover states
  --color-bg: #1b1b1b;                             // Page background
  --color-bg-secondary: #222;                      // Sidebar, cards, code blocks
  --color-bg-tertiary: #2a2a2a;                    // Borders, subtle separators
  --color-surface: #1a1a1a;                        // Feature cards, elevated surfaces
  --color-text: #d4d4d4;                           // Primary body text
  --color-text-muted: #888;                        // Secondary/helper text
  --color-text-heading: #f0f0f0;                   // Headings
  --color-accent-subtle: rgba(232, 89, 12, 0.08);  // Tinted backgrounds
  --color-accent-border: rgba(232, 89, 12, 0.2);   // Tinted borders
  --color-code-bg: #0d0d0d;                        // Inline and block code background
  --color-hero-gradient-start: #1b1b1b;            // Hero gradient
  --color-hero-gradient-end: #2d1b00;              // Hero gradient (warm)
}
```

## Deployment

### GitHub Actions workflow

```yaml
# .github/workflows/docs.yml
name: Deploy Docs
on:
  push:
    branches: [main]
    paths: [docs/site/**, crates/xtask/**]
  workflow_dispatch:

jobs:
  deploy:
    runs-on: ubuntu-latest
    permissions:
      pages: write
      id-token: write
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with: { cache-targets: false }
      - uses: taiki-e/install-action@v2
        with: { tool: zola }
      - run: cargo xtask gen-docs
      - run: cd docs/site && zola build
      - uses: actions/upload-pages-artifact@v3
        with: { path: docs/site/public }
      - uses: actions/deploy-pages@v4
```

### Base URL

```toml
# docs/site/config.toml
base_url = "https://tj-smith47.github.io/anodize"
```

## What This Spec Does NOT Cover

- Blog content or blog post templates (future work)
- Search integration (Zola has built-in elasticlunr.js search — can enable later)
- Custom domain setup (DNS/CNAME — user decision, not a code task)
- Light mode theme (dark-only for now, can add toggle later)
- Right-side table of contents (Zola supports it via `page.toc`, can add later)
