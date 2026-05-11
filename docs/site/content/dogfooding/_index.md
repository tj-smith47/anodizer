+++
title = "What works (with proof)"
description = "Every anodizer feature, with a status and a link you can click to see the working artifact: not source code, not test names, the actual file or page."
weight = 30
template = "section.html"
sort_by = "weight"
+++

# What works (with proof)

This page is the dogfood test for anodizer. Every feature has one of two
statuses, and the proof is always something you can open in your browser:
a release artifact, a published package, or a public registry entry. We
don't ask you to read source code to verify our claims.

## How to read this page

| Status | Means |
|---|---|
| ✅ **Verified** | anodize or cfgd ships with it. Click the link to see the public artifact. |
| 🤝 **Help wanted** | Tests pass. We can't validate the production path ourselves: paid account, missing runtime, or a target that doesn't fit our projects. Open an issue if you want to validate it on yours. |

Two public projects use anodizer to ship themselves:

- **anodizer**, releases at [github.com/tj-smith47/anodizer/releases](https://github.com/tj-smith47/anodizer/releases). Latest: [v0.1.1](https://github.com/tj-smith47/anodizer/releases/tag/v0.1.1).
- **cfgd**, a 4-crate workspace (CLI + lib + operator + CSI driver) at [github.com/tj-smith47/cfgd/releases](https://github.com/tj-smith47/cfgd/releases). Latest: [v0.3.5](https://github.com/tj-smith47/cfgd/releases/tag/v0.3.5).

When a row says "lives on `<package manager>`", click through and you'll
land on the live page. Where two examples exist (one per project), we link
both so you can see the same feature in two configurations.

## Where to look

| Section | What's in it |
|---|---|
| [Where you install it](install/) | Distribution channels users get the binary from |
| [What anodizer builds](build/) | Archives, packages, installers, containers, signing |
| [Release pipeline](release/) | Releases, changelogs, announcers, blob uploads, custom publishers |
| [anodizer.yml config](config/) | Top-level keys, templates, lifecycle hooks, monorepo |
| [CLI](cli/) | Commands and flags |
| [GitHub Action](action/) | anodizer-action inputs |
| [Rust-specific extras](rust/) | Features with no GoReleaser equivalent |

## Methodology

- **Reference target:** [GoReleaser](https://goreleaser.com/) (OSS + Pro). We
  track every documented feature in both editions plus our own Rust-specific
  additions.
- **Verified ✅:** anodize or cfgd ships with it. Public artifact at the
  linked URL (release file, package on a registry, image on GHCR).
- **Help wanted 🤝:** the feature is implemented and tested. We can't run
  the production path: paid account, missing runtime, or a target that
  doesn't fit either of our two projects.

If you can produce a public artifact for any 🤝 row, open a PR with the
link and we'll flip it to ✅. Same for any feature you think is missing
and should be ✅: send the proof. Open an issue if you want to validate
a 🤝 row against your own project.
