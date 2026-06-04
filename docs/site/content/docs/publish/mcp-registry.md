+++
title = "MCP registry"
description = "Publish an MCP server manifest to the Model Context Protocol registry"
weight = 12
template = "docs.html"
+++

Anodizer publishes a [Model Context Protocol](https://modelcontextprotocol.io/) server manifest to the public registry at [registry.modelcontextprotocol.io](https://registry.modelcontextprotocol.io), letting MCP-capable clients discover and install your server. The manifest describes how to fetch the server (OCI image, npm tarball, PyPI wheel, NuGet package, or `.mcpb` bundle) and which transport(s) it speaks. Configured under a top-level `mcp:` key, mirroring GoReleaser's `mcp_registries` pipe.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | false | warn-only (no programmatic unpublish; manual mark-deprecated via registry admin UI) | `MCP_GITHUB_TOKEN` or OIDC id-token (`id-token: write`) |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Manager rollback semantics.

## The `required:` field

Default: **`false`** — an MCP registry publish failure is logged but does not fail the release.

Set `required: true` to make the release exit non-zero if this publisher fails:

```yaml
mcp:
  name: io.github.myorg/myapp
  description: "A fast MCP server"
  packages:
    - registry_type: oci
      identifier: ghcr.io/myorg/myapp
      transport:
        type: stdio
  required: true
```

See [Publish overview — the `required:` field](../) for the full semantics.

## Minimal config

```yaml
mcp:
  name: io.github.myorg/myapp
  description: "A fast MCP server for managing things"
  packages:
    - registry_type: oci
      identifier: ghcr.io/myorg/myapp
      transport:
        type: stdio
```

This publishes anonymously (`auth.type: none`) to the default registry. For server names under the `io.github.<owner>` namespace you almost always want `auth.type: github-oidc` so the registry can verify ownership of the GitHub repo. See [Authentication](#authentication).

## Wiring the OCI image

When `packages[].registry_type: oci`, the MCP manifest *references* an image
but does not build one — anodize publishes the manifest, then MCP clients
`docker pull` the image at the referenced coordinate (`ghcr.io/myorg/myapp:<ver>`).
The image itself comes from `docker_v2:`:

```yaml
crates:
  - name: myapp
    docker_v2:
      - dockerfile: Dockerfile
        images: ["ghcr.io/myorg/myapp"]
        tags: ["{{ Version }}", "latest"]
        platforms: [linux/amd64, linux/arm64]
        labels:
          io.modelcontextprotocol.server.name: io.github.myorg/myapp   # REQUIRED — must equal mcp.name

mcp:
  name: io.github.myorg/myapp
  packages:
    - registry_type: oci
      identifier: ghcr.io/myorg/myapp     # matches docker_v2[].images
      transport:
        type: stdio
  auth:
    type: github-oidc
```

> **Required OCI label.** The registry's OCI validator reads the image-config
> label `io.modelcontextprotocol.server.name` (i.e. `Config.Labels`) and
> **rejects the publish** if it is missing or does not equal `mcp.name`. It
> MUST be set as a `docker_v2[].labels` entry — the registry **ignores**
> `annotations:`, so an annotation will not satisfy the check.

The release-workflow permissions must cover both:

```yaml
permissions:
  contents: write
  packages: write    # docker_v2 push to ghcr.io
  id-token: write    # mcp.auth.type: github-oidc
```

See [Docker (`docker_v2:`)](../packages/docker.md) for the image build
surface and the Dockerfile pattern that makes the container speak MCP over
stdio out of the box (`ENTRYPOINT ["/usr/local/bin/myapp"] / CMD ["mcp"]`).

## Full config reference

```yaml
mcp:
  name: io.github.myorg/myapp       # required; fully-qualified server name (template)
  title: ""                          # optional; human-readable title (template)
  description: ""                    # optional; one-line description (template)
  homepage: ""                       # optional; project URL (template)
  skip: false                        # optional; bool or template string
  registry: "https://registry.modelcontextprotocol.io"  # optional; override endpoint
  repository:                        # optional; url + source derived from the GitHub origin remote
    url: "https://github.com/myorg/myapp"   # derived from `git remote get-url origin` (GitHub only)
    source: github                   # github | gitlab | gitea; derived as `github` only for a GitHub remote
    id: ""                           # optional; source-specific repo ID (left blank — no blocking API call)
    subfolder: ""                    # optional; monorepo subfolder
  packages:                          # required; one or more distribution packages
    - registry_type: oci             # oci | npm | pypi | nuget | mcpb
      identifier: ghcr.io/myorg/myapp  # package coordinate (template)
      transport:
        type: stdio                  # stdio | streamable-http | sse
  transports:                        # optional; accepted for GoReleaser portability
    - type: stdio
  auth:
    type: none                       # none | bearer | github-oidc
    # For bearer:
    # token: "{{ Env.MCP_TOKEN }}"
    # For github-oidc: no extra fields needed; token is auto-obtained from Actions
```

## Authentication

`auth.type` controls how anodizer authenticates the `POST /v0/publish` call. Default is `none`.

| `auth.type` | When to use | Token source |
|-------------|-------------|--------------|
| `none` | Server names without an ownership namespace; private mirrors | None, or `auth.token` for a static bearer |
| `github` | Names under `io.github.<owner>/...` published from a workstation or non-GHA CI | GitHub PAT in `auth.token` (or `MCP_GITHUB_TOKEN` env). Anodizer exchanges it for a short-lived registry token |
| `github-oidc` | Names under `io.github.<owner>/...` published from GitHub Actions | GHA-native OIDC id-token (`ACTIONS_ID_TOKEN_REQUEST_TOKEN` + `ACTIONS_ID_TOKEN_REQUEST_URL`). Anodizer exchanges it for a short-lived registry token. No PAT required |

### `auth.type: none`

```yaml
mcp:
  name: io.github.myorg/myapp
  auth:
    type: none
  # ...
```

Used for private registries that gate writes by network or a separate bearer:

```yaml
mcp:
  registry: https://mcp-mirror.internal.example.com
  auth:
    type: none
    token: "{{ Env.INTERNAL_MCP_BEARER }}"
```

### `auth.type: github` (PAT)

```yaml
mcp:
  name: io.github.myorg/myapp
  auth:
    type: github
    token: "{{ Env.GITHUB_TOKEN }}"
```

The PAT only needs `read:user` scope. Anodizer calls the registry's GitHub-token exchange endpoint and receives a short-lived bearer it uses for `POST /v0/publish`.

### `auth.type: github-oidc` (GitHub Actions native)

```yaml
mcp:
  name: io.github.myorg/myapp
  auth:
    type: github-oidc
```

In a workflow, give the job permission to mint the id-token:

```yaml
permissions:
  contents: write
  id-token: write    # required for github-oidc

jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
      - uses: tj-smith47/anodizer-action@v1
        with:
          args: release --clean
```

Anodizer reads the OIDC request URL and token from the standard GHA env vars, requests an id-token scoped to the registry's audience, exchanges it for a short-lived registry bearer, and publishes.

## Common gotchas

- **`packages` is required**: omitting `packages` causes a config validation error; an empty list is rejected by the registry.
- **OCI version field**: OCI packages get an empty `version` field in the published manifest — the registry resolves the version from the image tag. Other registry types get the release version verbatim.
- **`top-level transports`**: the `transports:` list is accepted for GoReleaser config-portability but is not emitted to the registry. Transports are per-package via `packages[].transport`.
- **Ownership verification**: for server names under `io.github.<owner>/...`, use `auth.type: github-oidc` (in GHA) or `auth.type: github` (PAT). The registry uses this to verify you own the GitHub repository.

## MCP config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | **required** | Fully-qualified server name (e.g. `io.github.OWNER/PROJECT`). Supports templates |
| `title` | string | none | Human-readable title shown in registry UIs. Supports templates |
| `description` | string | none | One-line description. Supports templates |
| `homepage` | string | none | Project homepage URL. Supports templates |
| `skip` | string or bool | `false` | Skip this publisher. Tera template that evaluates to a truthy value (e.g. `"{{ true }}"`) also skips. Accepts the legacy `disable:` spelling for back-compat with imported GoReleaser configs |
| `repository` | object | inferred | Source repository metadata. See [Repository](#repository) |
| `packages` | object[] | **required** | One or more distribution packages. See [Packages](#packages) |
| `transports` | object[] | none | Top-level transport list. Parsed for GoReleaser config-portability (silently ignored — see [note below](#top-level-transports)); the current MCP server schema derives transports per-package via `packages[].transport`, so this list is not emitted to the registry |
| `auth` | object | `{type: none}` | Registry authentication. See [Authentication](#authentication) |
| `registry` | string | `https://registry.modelcontextprotocol.io` | Override the registry endpoint (for staging or a private mirror) |

The top-level `retry:` config applies: `POST /v0/publish` calls inherit anodizer's standard retry policy (backoff, max attempts, retryable status codes).

### Repository

```yaml
repository:
  url: https://github.com/myorg/myapp
  source: github          # github | gitlab | gitea
  id: ""                  # optional, source-specific repo ID
  subfolder: ""           # optional, e.g. "servers/myapp" inside a monorepo
```

All fields support Tera templates. The whole `repository` block is optional:
when omitted, anodizer derives `url` and `source` from the `origin` git remote.
Derivation is GitHub-only — a non-GitHub remote (self-hosted, GitLab) is left
for you to supply rather than forced to a wrong `source`. A user-set
`repository` always wins.

### Packages

Each entry describes one downloadable form of the server. List all the ones you publish; clients pick the best match for their host.

```yaml
packages:
  - registry_type: oci            # oci | npm | pypi | nuget | mcpb
    identifier: ghcr.io/myorg/myapp
    transport:
      type: stdio                 # stdio | streamable-http | sse
```

| Field | Type | Description |
|-------|------|-------------|
| `registry_type` | string | One of `oci`, `npm`, `pypi`, `nuget`, `mcpb` |
| `identifier` | string | Package coordinate. Supports templates. For OCI: `ghcr.io/owner/img`. For npm: `@scope/name`. For PyPI: distribution name. For NuGet: package ID. For mcpb: download URL |
| `transport.type` | string | `stdio`, `streamable-http`, or `sse` |

When `registry_type: oci`, the published manifest carries an empty `version` field on the package entry (the registry resolves the image tag itself). Other registry types receive the release version verbatim. This mirrors GoReleaser's `mcp_registries` behavior.

The OCI image referenced by `identifier` MUST carry the image-config label
`io.modelcontextprotocol.server.name` set to the value of `mcp.name`. The
registry's OCI validator reads `Config.Labels` and rejects the publish without
it. Set it via `docker_v2[].labels` (NOT `annotations:`, which the registry
ignores) — see [Wiring the OCI image](#wiring-the-oci-image).

### Top-level transports

```yaml
transports:
  - type: stdio
  - type: streamable-http
```

Optional. The `transports:` list is accepted for GoReleaser config-portability (so a migrated `.goreleaser.yaml` doesn't error on `deny_unknown_fields`); the current MCP server schema derives transports per-package via `packages[].transport`, so this list is not emitted to the registry.

## Skipping per release

`skip` accepts either a bool or a Tera template:

```yaml
mcp:
  # ...
  skip: "{{ if Prerelease }}true{{ endif }}"
```

Common patterns:

| Goal | Value |
|------|-------|
| Always skip | `skip: true` or `skip: "{{ true }}"` |
| Skip pre-releases | `skip: "{{ if Prerelease }}true{{ endif }}"` |
| Skip snapshot builds | `skip: "{{ if IsSnapshot }}true{{ endif }}"` |

## Full example

```yaml
mcp:
  name: io.github.myorg/myapp
  title: "My MCP Server"
  description: "Provides filesystem and shell tools over MCP"
  homepage: "https://github.com/myorg/myapp"
  repository:
    url: "https://github.com/myorg/myapp"
    source: github
  packages:
    - registry_type: oci
      identifier: ghcr.io/myorg/myapp
      transport:
        type: stdio
    - registry_type: npm
      identifier: "@myorg/myapp-mcp"
      transport:
        type: stdio
  transports:
    - type: stdio
  auth:
    type: github-oidc
  skip: "{{ if Prerelease }}true{{ endif }}"
```

## Templating

Tera templates are evaluated on these fields before publish: `name`, `title`, `description`, `homepage`, every `repository.*` field, every `auth.*` field, and each `packages[].identifier`. The standard anodizer template context applies (`Version`, `ProjectName`, `Env.*`, `Prerelease`, etc.). See [Templates](@/docs/general/templates.md).

## Dry-run

`anodizer release --dry-run` renders the full manifest and logs the intended POST without contacting the registry, useful for verifying the package list and transport fields before a real publish.

## Migrating from GoReleaser

The top-level YAML key is identical (`mcp:` in both). GoReleaser's deprecated nested `mcp.github:` block (from older configs) collapses to top-level `mcp.*` fields in anodizer — write top-level fields directly. See the [GoReleaser migration guide](@/migration/goreleaser.md) for the full mapping.
