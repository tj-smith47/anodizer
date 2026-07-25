+++
title = "What works (with proof)"
description = "Every anodizer feature, with a status and a link you can click to see the working artifact: not source code, not test names, the actual file or page."
weight = 30
template = "section.html"
sort_by = "weight"
+++

# What works (with proof)

This page is the dogfood test for anodizer. Every feature carries a status and
a link, and the status tells you exactly what the link proves.

## How to read this page

| Status | Means |
|---|---|
| ✅ **Verified** | The feature runs on real releases of anodizer, cfgd, or brontes. |
| ✅ **Verified (tests)** | Implemented and covered by tests; no live release has exercised the code path. The link points at the implementation or its tests. |
| ⏳ **Pending** | Wired into a live config, but the code path only fires on a condition no release has hit yet: a failure branch, an override nothing passes, an output no workflow consumes, or an upstream gate we're waiting on. |
| 🤝 **Help wanted** | Tests pass. We can't run the production path ourselves: a paid account, a missing runtime, or a target that doesn't fit any of our three projects. Open an issue if you want to validate it on yours. |
| ⛔ **Removed (date)** | The surface was deleted. The row stays, with the run evidence that motivated the removal, so the history isn't rewritten. |

### What the link is

A ✅ **Verified** row links the strongest evidence that exists for that feature,
in this order:

1. **A public artifact** — a release asset, a package page, a registry entry, an
   image tag. Roughly a quarter of the ✅ rows have one, and it is always
   preferred.
2. **The live config or workflow that ran it**, for features that leave no
   separate artifact of their own: a checksum algorithm, a template variable, a
   retry budget, a publisher gate. The linked `.anodizer.yaml` or workflow file
   is the one that executed on the releases listed below; where a specific run
   demonstrated it, the run URL is cited alongside.
3. **The implementation, plus the run log line it emitted**, for behavior that
   is only observable in a release's output — a rollback that fired, a gate that
   skipped a publisher, a warning that surfaced.

So: a ✅ row always means *this ran*. It does not always mean *there is a file
you can download because it ran* — many of these features exist precisely so
that no extra file appears. Where the only evidence is a test, the status says
**Verified (tests)** instead, and never ✅ Verified.

### Config mode

anodizer runs in three configuration modes and a few features behave
differently in each, so those rows name the mode they were proven in:

| Mode | Shape | Proven by |
|---|---|---|
| **Lockstep** | one workspace, all crates share a version and one `v<version>` tag | anodizer |
| **Per-crate** | `workspaces:` entries with independent versions, tags, and cadences | cfgd |
| **Single-crate** | one crate, no workspace | brontes |

Rows without a mode annotation behave identically in all three.

Three public projects use anodizer to ship themselves:

- **anodizer**, a lockstep workspace (every crate shares one version and one `v<version>` tag) at [github.com/tj-smith47/anodizer/releases](https://github.com/tj-smith47/anodizer/releases). Latest: [v0.23.0](https://github.com/tj-smith47/anodizer/releases/tag/v0.23.0).
- **cfgd**, a per-crate workspace (CRD + lib + CLI + operator + CSI driver, five `workspaces:` entries on independent cadences) at [github.com/tj-smith47/cfgd/releases](https://github.com/tj-smith47/cfgd/releases). Latest: [v0.6.1](https://github.com/tj-smith47/cfgd/releases/tag/v0.6.1).
- **brontes**, a single crate (clap → MCP server toolkit) at [github.com/tj-smith47/brontes/releases](https://github.com/tj-smith47/brontes/releases). Latest: [v0.3.0](https://github.com/tj-smith47/brontes/releases/tag/v0.3.0). Library-only pipeline: changelog → source tarball → source SBOM → keyless sign → attestation → cargo publish.

When a row says "lives on `<package manager>`", click through and you'll
land on the live page. Where multiple examples exist (one per project), we
link each so you can see the same feature in different configurations —
lockstep workspace (anodizer), per-crate workspace (cfgd), and single crate
(brontes).

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
- **Verified ✅:** anodizer, cfgd, or brontes ships with it. Public artifact
  at the linked URL (release file, package on a registry, image on GHCR).
- **Help wanted 🤝:** the feature is implemented and tested. We can't run
  the production path: paid account, missing runtime, or a target that
  doesn't fit any of our three projects.
- **Historical pins:** when a feature was dogfooded on a past release but a
  project has since moved off it, the proof link stays pinned to the tag
  that exercised it (marked "dogfooded through vX.Y.Z") — never silently
  dropped, never re-pointed at a master file that no longer proves it.

If you can produce a public artifact for any 🤝 row, open a PR with the
link and we'll flip it to ✅. Same for any feature you think is missing
and should be ✅: send the proof. Open an issue if you want to validate
a 🤝 row against your own project.
