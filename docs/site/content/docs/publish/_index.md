+++
title = "Publish"
weight = 60
sort_by = "weight"
template = "section.html"
+++

## The `required:` field

Every publisher block accepts a `required:` field that controls whether a failure
from that publisher fails the overall release.

```yaml
homebrew_casks:
  - repository:
      owner: myorg
      name: homebrew-tap
    required: true   # release fails if the Homebrew cask push fails
```

`required:` sits on every publisher block the same way — the example above
happens to show the top-level `homebrew_casks` array, but a per-crate
`publish.scoop`, `publish.cargo`, or `publish.homebrew_cask` block takes it
identically.

### Behavior

| Value | Effect |
|-------|--------|
| `true` | Failure here causes the release to exit non-zero. |
| `false` | Failure is logged but the release continues. |
| omitted (default) | Falls through to the publisher's hardcoded default. |

### Per-publisher defaults

| Publisher | Default | Rationale |
|-----------|---------|-----------|
| GitHub Releases (`release:`) | `true` | Core delivery artifact; a failed release upload is always a blocker. |
| crates.io (`cargo`) | `true` | Registry publish is the primary artifact for library crates. |
| All others | `false` | Secondary distribution channels; partial failures should not block the release. |

Each per-publisher page lists its default in the Classification table and includes a
`required:` snippet in the config reference.

Other publishers — [Homebrew Casks](./homebrew-casks.md) (the canonical Homebrew
publisher; [deprecated Formula](./homebrew.md) is documented separately),
[Homebrew Core](./homebrew-core.md) (bump an existing `homebrew-core` formula), [Scoop](./scoop.md),
[Chocolatey](./chocolatey.md), [Winget](./winget.md), [AUR](./aur.md), [Krew](./krew.md),
[MCP registry](./mcp-registry.md), [SchemaStore](./schemastore.md), [crates.io](./crates-io.md),
[NPM](./npm.md), [PyPI](./pypi.md), [Docker Hub](./dockerhub.md), and others — are documented in their own pages.

### Submitter publishers

Chocolatey, winget, and AUR Sources are _submitter_ publishers: they push to an
external moderation queue whose outcome is asynchronous. The submit itself succeeds
when the queue accepts it; approval and going live come later.

`required: true` on a submitter publisher fails the release when the submission
itself fails. The moderation outcome (queue rejection) arrives days after the release
completes, so no setting can gate the release on it. With `-v`, anodizer prints an
advisory saying so when it loads a config that sets `required: true` on one of these
publishers; the per-publisher pages quote the line.
