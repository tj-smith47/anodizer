+++
title = "crates.io"
description = "Publish crates to the Rust package registry"
weight = 2
template = "docs.html"
+++

Publish your crate to [crates.io](https://crates.io) with dependency-aware ordering.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Submitter | true | `cargo yank` (version stays reserved; consumers cannot install fresh) | `CARGO_REGISTRY_TOKEN yank` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## Minimal config

```yaml
crates:
  - name: myapp
    publish:
      cargo: {}     # presence opts in (no `enabled` field, no bool shorthand)
```

To opt out without removing the block, use `skip:` (peer-publisher convention):

```yaml
publish:
  cargo:
    skip: true
```

## Full config reference

```yaml
publish:
  cargo:
    index_timeout: 300        # seconds to wait for crates.io index update
    registry: ""              # alternative registry name from ~/.cargo/config.toml
    index: ""                 # alternative registry index URL
    no_verify: false          # skip local cargo build verification
    allow_dirty: true         # default true (anodize tag dirties the tree)
    features: []              # features to enable
    all_features: false
    no_default_features: false
    target: ""                # target triple override
    target_dir: ""            # cargo target directory override
    jobs: 0                   # parallelism (0 = cargo default)
    keep_going: false
    manifest_path: ""         # Cargo.toml path override
    locked: false
    offline: false
    frozen: false
    skip: false               # bool, "true"/"false", or "auto"
```

## Authentication

Set `CARGO_REGISTRY_TOKEN`:

```bash
export CARGO_REGISTRY_TOKEN="cio_..."
anodizer release
```

## Common gotchas

- **Version slot burned permanently**: once a version is published to crates.io it cannot be deleted — only yanked. A yanked version stays reserved; consumers with an explicit version pin can still install it, but `cargo add` and `cargo update` will not select it. Plan releases carefully before pushing.
- **Index lag**: crates.io's index update can take 30–120 seconds after publish. Anodizer waits up to `index_timeout` seconds (default 300) before publishing dependent crates in the workspace ordering chain.
- **`allow_dirty: true`** is the default because `anodize tag` writes a version bump commit that leaves the tree dirty. Set `allow_dirty: false` only if you manage version bumps externally.

## Republish / update behavior

Not applicable — crates.io does not allow re-publishing a version once it has been published. Each release must use a new version number. Use `cargo yank` (via the rollback path) to prevent new installs of a broken version.

## Config options

```yaml
publish:
  cargo:
    # ----- crates.io–specific (anodizer-original) -----
    index_timeout: 300        # seconds to wait for crates.io index update

    # ----- registry selection -----
    registry: my-alt-registry # name from ~/.cargo/config.toml
    index: https://...        # registry index URL

    # ----- verify / dirty -----
    no_verify: false          # skip the local cargo build verification
    allow_dirty: true         # default: true (anodize tag dirties the tree)

    # ----- features -----
    features: ["telemetry"]
    all_features: false
    no_default_features: false

    # ----- compilation -----
    target: x86_64-unknown-linux-gnu
    target_dir: ./target
    jobs: 4
    keep_going: false

    # ----- manifest -----
    manifest_path: ./Cargo.toml
    locked: true
    offline: false
    frozen: false

    # ----- peer-publisher pattern -----
    skip: false               # template-aware: bool, "true"/"false", or "auto"
```

## Workspace ordering

When publishing multiple workspace crates, anodizer resolves dependency order using topological sorting. If crate `B` depends on crate `A`, `A` is published first and anodizer waits for the crates.io index to update before publishing `B`.
