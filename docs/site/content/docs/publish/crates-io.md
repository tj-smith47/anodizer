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

## The `required:` field

Default: **`true`** — a crates.io publish failure fails the release.

Set `required: false` to log failures but continue:

```yaml
crates:
  - name: myapp
    publish:
      cargo:
        required: false   # continue release even if crates.io publish fails
```

See [Publish overview — the `required:` field](../) for the full semantics.

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
    allow_dirty: true         # default true (anodizer tag dirties the tree)
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

The `auth` field selects how anodizer authenticates the publish:

| `auth` | Behaviour |
|---|---|
| `auto` (default) | Uses `CARGO_REGISTRY_TOKEN` when it is set; otherwise, under GitHub Actions with `id-token: write`, mints a short-lived token via Trusted Publishing. Errors only when neither is available. |
| `token` | Always uses `CARGO_REGISTRY_TOKEN`. anodizer's historical behaviour. |
| `oidc` | Always uses Trusted Publishing; never falls back to a stored token. Fails loudly if the GitHub Actions OIDC request env is absent. |

### Token

Set `CARGO_REGISTRY_TOKEN`:

```bash
export CARGO_REGISTRY_TOKEN="cio_..."
anodizer release
```

### Trusted Publishing (OIDC)

anodizer publishes to crates.io without a stored `CARGO_REGISTRY_TOKEN` by exchanging a GitHub Actions OIDC identity for a short-lived crates.io token — the same Trusted Publishing model anodizer offers for PyPI. Register a Trusted Publisher for each crate on crates.io (Settings → Trusted Publishing) with this repository and the **workflow file that actually runs the publish**, grant the job `id-token: write`, and set `auth: oidc`:

```yaml
publish:
  cargo:
    auth: oidc
```

```yaml
# publish-oidc.yml — the workflow named in the crates.io Trusted-Publisher config
permissions:
  id-token: write   # required — lets the runner request the OIDC id-token
  contents: read
```

**The publish must run on an accepted trigger.** crates.io Trusted Publishing accepts
only `push`, `release`, and `workflow_dispatch` — it **rejects `workflow_run`** with
`400 "does not support the workflow_run event trigger"`. The OIDC `event_name` claim is
fixed per workflow-run, so if your release workflow is triggered by `workflow_run` (as
anodizer's `release.yml` is, after CI), the cargo publish cannot run inside it. Anodizer
solves this by running the OIDC publishers from a standalone **`publish-oidc.yml`**
(`on: workflow_dispatch`), which `release.yml` dispatches after its main publish and
waits on. Register the Trusted Publisher against **`publish-oidc.yml`**, not
`release.yml`. If your release workflow is `push`- or `release`-triggered, no split is
needed — name that workflow directly. The same constraint applies to a reusable
`workflow_call` workflow: it inherits the caller's event and cannot be a Trusted
Publisher, so the OIDC publish must live in a standalone `workflow_dispatch` workflow.

anodizer mints **one** token before the dependency-order publish loop, injects it into every `cargo publish` via `CARGO_REGISTRY_TOKEN` (never on the command line), and revokes it after the loop — the token is workspace-scoped, so a single mint authorizes every crate whose Trusted-Publisher config matches this repository/workflow. A minted token also self-expires in ~30 minutes, so even a failed revoke leaves nothing long-lived behind. Trusted Publishing targets crates.io only; an `oidc` block against a custom `registry:`/`index:` is a config error — use a token there.

## Common gotchas

- **Version slot burned permanently**: once a version is published to crates.io it cannot be deleted — only yanked. A yanked version stays reserved; consumers with an explicit version pin can still install it, but `cargo add` and `cargo update` will not select it. Plan releases carefully before pushing.
- **Index lag**: crates.io's index update can take 30–120 seconds after publish. Anodizer waits up to `index_timeout` seconds (default 300) before publishing dependent crates in the workspace ordering chain.
- **`allow_dirty: true`** is the default because `anodizer tag` writes a version bump commit that leaves the tree dirty. Set `allow_dirty: false` only if you manage version bumps externally.

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
    allow_dirty: true         # default: true (anodizer tag dirties the tree)

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

    # ----- authentication -----
    auth: auto                # auto | token | oidc (Trusted Publishing)

    # ----- peer-publisher pattern -----
    skip: false               # template-aware: bool, "true"/"false", or "auto"
```

## Workspace ordering

When publishing multiple workspace crates, anodizer resolves dependency order using topological sorting. If crate `B` depends on crate `A`, `A` is published first and anodizer waits for the crates.io index to update before publishing `B`.
