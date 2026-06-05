+++
title = "Publish"
sort_by = "weight"
template = "section.html"
+++

## The `required:` field

Every publisher block accepts a `required:` field that controls whether a failure
from that publisher fails the overall release.

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        repository:
          owner: myorg
          name: homebrew-tap
        required: true   # release fails if the Homebrew push fails
```

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

Other publishers — [Homebrew](./homebrew.md), [Scoop](./scoop.md), [Chocolatey](./chocolatey.md),
[Winget](./winget.md), [AUR](./aur.md), [Krew](./krew.md), [MCP registry](./mcp-registry.md),
[SchemaStore](./schemastore.md), [crates.io](./crates-io.md), [NPM](./npm.md),
[Docker Hub](./dockerhub.md), and others — are documented in their own pages.

### Submitter publishers

Chocolatey, winget, and AUR Sources are _submitter_ publishers: they push to an
external moderation queue whose outcome is asynchronous. The submit itself succeeds
at queue acceptance time — not when the package is approved and live.

Setting `required: true` on a submitter publisher has no meaningful effect because
the failure mode it guards against (queue rejection) happens days after the release
completes. Anodizer emits a `tracing::warn` at config-validation time if `required: true`
is set on one of these publishers. See the per-publisher pages for the exact warning
message.
