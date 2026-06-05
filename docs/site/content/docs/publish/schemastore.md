+++
title = "SchemaStore"
description = "Register and refresh your tool's JSON schemas on SchemaStore at release time"
weight = 13
template = "docs.html"
+++

Anodizer registers and refreshes your tool's JSON schema(s) on [SchemaStore](https://www.schemastore.org/) at release time, opening a PR against your fork of `SchemaStore/schemastore`. Once merged, editors that consume SchemaStore (VS Code, JetBrains IDEs, Neovim, etc.) automatically offer validation and autocomplete for your tool's config files.

SchemaStore registration is always a PR with CI + human codeowner review — auto-merge is impossible. Anodizer opens (or refreshes) the PR; a SchemaStore maintainer merges it.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | false (collapsed across `schemas[]`) | close PR (or `git revert` + push the fork branch) | `GITHUB_TOKEN pull_request:write` (fork push + upstream PR) |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Manager rollback semantics.

## The two modes

Field presence selects the mode. Set **exactly one** of `url` or `schema_file` per entry:

| Field | Mode | What lands in SchemaStore |
|-------|------|--------------------------|
| `url` | **External** | Catalog entry only — points at a URL you host |
| `schema_file` | **Vendor** | Schema file copied to `src/schemas/json/<slug>.json` + catalog entry |

### External mode

anodizer adds or refreshes only the catalog entry in `catalog.json`. The schema file lives at a URL you host (e.g. a GitHub Pages site, release assets). On every subsequent release, zero SchemaStore changes are required — your URL always serves the latest schema.

```yaml
schemastore:
  repository:
    owner: tj-smith47
    name: schemastore
  schemas:
    - name: Anodizer
      file_match: [".anodizer.yaml", ".anodizer.yml"]
      url: "https://tj-smith47.github.io/anodizer/schema.json"
      description: "Anodizer Rust release-automation configuration file"
```

This is anodizer's own dogfood — the entry at `SchemaStore/schemastore#5727` was originally hand-submitted; this publisher keeps it fresh automatically.

### Vendor mode

anodizer copies your schema file into the SchemaStore repository at `src/schemas/json/<slug>.json`, and sets the catalog `url` to `https://www.schemastore.org/<slug>.json`. Each release re-vendors the file in the same PR.

```yaml
schemastore:
  repository:
    owner: tj-smith47
    name: schemastore
  schemas:
    - name: cfgd-config
      slug: cfgd-config
      file_match: ["cfgd.yaml", ".cfgd.yaml"]
      schema_file: "schemas/cfgd-config.schema.json"
      crate: cfgd
      description: "cfgd machine configuration"
```

cfgd uses vendor mode for its config schemas — the driving consumer for this publisher.

## Minimal config

```yaml
schemastore:
  repository:
    owner: myorg
    name: schemastore
  schemas:
    - name: MyTool
      file_match: [".mytool.yaml", ".mytool.yml"]
      url: "https://myorg.github.io/mytool/schema.json"
```

`repository` and `file_match` are required. `url` or `schema_file` is required (exactly one). Everything else derives from project metadata.

## The `required:` field

Default: **`false`** — a SchemaStore PR failure is logged but does not fail the release. `required` is collapsed across all entries: if any entry sets `required: true`, the whole publisher is required.

```yaml
schemastore:
  repository:
    owner: myorg
    name: schemastore
  schemas:
    - name: MyTool
      file_match: [".mytool.yaml"]
      url: "https://myorg.github.io/mytool/schema.json"
      required: true
```

See [Publish overview — the `required:` field](../) for the full semantics.

## Block-level defaults and per-entry overrides (the cascade)

`repository`, `commit_author`, `versioned`, `skip`, and `if` set at the block level are defaults for every entry. A per-entry field overrides the block default:

```yaml
schemastore:
  repository:
    owner: myorg
    name: schemastore
  versioned: false                # default for all entries

  schemas:
    - name: MyTool
      file_match: [".mytool.yaml"]
      schema_file: "schemas/mytool.schema.json"
      # inherits versioned: false

    - name: MyTool Legacy
      file_match: [".mytool-v1.yaml"]
      schema_file: "schemas/mytool-v1.schema.json"
      versioned: true             # overrides the block default
```

Resolution order (most-specific wins): **per-entry field → block `schemastore.*` field → derived from project metadata**.

## Config fields reference

### Block-level fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `repository` | object | **required** | Fork of `SchemaStore/schemastore` to push to and open the PR from. Supports `owner`, `name`, `token`, `branch`, `git`, and `pull_request` |
| `commit_author` | object | git config | Commit author `name` and `email` |
| `versioned` | bool | `false` | Default for all entries. Vendor-only. See [`versioned`](#versioned) |
| `skip` | bool or string | `false` | Skip the whole publisher. Accepts bool or Tera template string. Alias: `disable` |
| `if` | string | — | Tera condition; publisher is skipped when it renders falsy |
| `schemas` | list | **required** | The schema entries to register or refresh. At least one required |

### Per-entry fields

| Field | Required? | Default | Description |
|-------|-----------|---------|-------------|
| `name` | yes | — | Catalog display name (may be Title Case, e.g. `Anodizer`) |
| `slug` | no | `name` slugified | Vendor filename / url basename. Vendor-only |
| `file_match` | yes | — | Well-known config filenames this schema validates. Cannot be derived |
| `url` | one of `url`/`schema_file` | — | Sets **external** mode. The URL you host the schema at |
| `schema_file` | one of `url`/`schema_file` | — | Sets **vendor** mode. Repo-root-relative path to your schema file |
| `crate` | no | primary crate | Binds version scope to a specific crate (per-crate workspace mode). Vendor/versioned only |
| `description` | no | derived from project metadata | Catalog description. Must not contain the word "schema". See [SchemaStore content rules](#schemastore-content-rules) |
| `versioned` | no | block default | Emit a version-suffixed vendored file + `versions` map. Vendor-only |
| `required` | no | `false` | Collapse across all entries via escalate-to-true: one `required: true` makes the whole publisher required |
| `skip` | no | `false` | Per-entry skip. Bool or Tera template string. Alias: `disable` |
| `if` | no | — | Per-entry Tera condition |

## `file_match`

Lists the well-known config filenames the schema validates. Used verbatim in the SchemaStore catalog entry. Folder globs must start with `**/`:

```yaml
schemas:
  - name: MyTool
    file_match:
      - ".mytool.yaml"
      - ".mytool.yml"
      - "**/modules/*.yaml"    # folder glob — requires **/ prefix
    url: "https://myorg.github.io/mytool/schema.json"
```

`file_match` is always required — there is no default.

## `crate` (workspace per-crate mode)

In a workspace with per-crate independent versions, `crate:` binds a vendored or versioned schema's version to a specific crate's tag rather than the first-crate fallback:

```yaml
schemastore:
  repository:
    owner: myorg
    name: schemastore
  schemas:
    - name: myapp-config
      file_match: ["myapp.yaml"]
      schema_file: "crates/myapp/schemas/config.schema.json"
      crate: myapp              # version from myapp's tag, not workspace root

    - name: myplugin-config
      file_match: ["myplugin.yaml"]
      schema_file: "crates/myplugin/schemas/config.schema.json"
      crate: myplugin           # version from myplugin's tag
```

In single-crate and workspace-lockstep modes, `crate:` is optional and defaults to the primary crate.

## `versioned`

Vendor-only. When `true`, anodizer writes a version-suffixed file (`<slug>-<VER>.json`) alongside the base file, and merges the new version into the catalog entry's `versions` map — carrying prior versions forward so editors that locked to an older version still resolve:

```yaml
schemas:
  - name: MyTool
    slug: mytool
    file_match: [".mytool.yaml"]
    schema_file: "schemas/mytool.schema.json"
    versioned: true
```

This writes `src/schemas/json/mytool-1.2.3.json` and adds `"1.2.3"` to the `versions` map in the catalog entry. Prior version keys are preserved — the PR merges into whatever was already in the upstream entry.

## `slug`

Vendor-only. Controls the filename in `src/schemas/json/<slug>.json` and the `url` basename in the catalog (`https://www.schemastore.org/<slug>.json`). Defaults to the `name` slugified (lowercased, spaces replaced with `-`):

```yaml
schemas:
  - name: My Tool Config      # would slug to "my-tool-config"
    slug: mytool               # override: src/schemas/json/mytool.json
    file_match: [".mytool.yaml"]
    schema_file: "schemas/mytool.schema.json"
```

## SchemaStore content rules

The SchemaStore CI gates enforce these rules — get any wrong and the PR is red. Anodizer validates all of these at preflight before opening the PR:

### `description`

- **Required and non-empty.** Anodizer derives a description from your project/crate metadata if you omit it; set it explicitly to override or when the derived text would violate the rules below.
- **Must not contain the substring `schema`** (case-insensitive). `"cfgd configuration schema file"` is rejected; `"cfgd machine configuration"` is accepted.
- **Must be single-line** (no newlines).
- **Must not start or end with** `, . <space> <tab> -`.

### `$id`

The schema's `$id` field must be an absolute `http(s)://` URL. Relative or urn-form IDs are rejected by SchemaStore CI.

### `$schema` dialect

- **Draft-04, draft-06, draft-07**: accepted unconditionally.
- **Draft 2019-09 or 2020-12**: allowed, but anodizer automatically adds the schema's `<slug>` to the `highSchemaVersion` allowlist in `src/schema-validation.jsonc` in the same PR. This keeps your schema as-authored; the allowlist entry satisfies SchemaStore CI.

> A failed `$schema` check on one entry fails the **entire PR**, including any good entries. Anodizer catches dialect mismatches at preflight so the PR lands clean.

## Authentication

Anodizer resolves a GitHub token from `repository.token`, or falls back to the `GITHUB_TOKEN` / `ANODIZER_FORCE_TOKEN` environment variables. The token needs push access to your fork (`contents:write`) and permission to open a pull request against the upstream `SchemaStore/schemastore` (`pull_request:write`).

See the anodizer-action docs for how to wire the fork token in GitHub Actions alongside other publisher tokens.

## `skip` and `if`

Both accept a bool or a Tera template string. The per-entry and block-level fields are OR-combined: if either is truthy, that schema is skipped.

```yaml
schemastore:
  if: "{{ not IsSnapshot }}"        # skip the whole publisher on snapshots
  schemas:
    - name: MyTool
      file_match: [".mytool.yaml"]
      url: "https://myorg.github.io/mytool/schema.json"
      skip: "{{ if Prerelease }}true{{ endif }}"   # also skip this entry on pre-releases
```

## Idempotency

Before opening a PR, anodizer checks whether the upstream `SchemaStore/schemastore:master` already has an identical entry (same `name`, same `url`, same vendored file bytes for vendor mode). If nothing changed, no PR is opened. This is the expected result when anodizer runs against its own config — the entry for `#5727` is already present and unchanged.

## Rollback

If the release fails after the SchemaStore PR is opened, anodizer closes it (and/or reverts the fork branch). Rollback is best-effort: if the PR was merged within the release window, closing is impossible. In that case a follow-up revert PR is required — anodizer logs a warning with the PR URL.

## Dry-run

`anodizer release --dry-run` renders the planned catalog diff (new or updated entries, any vendor files, `highSchemaVersion` additions) and logs the intended PR without cloning, committing, or pushing:

```
$ anodizer release --dry-run
[schemastore] would open PR: tj-smith47/schemastore → SchemaStore/schemastore:master
[schemastore] catalog entry: ADD Anodizer (external, url=https://tj-smith47.github.io/anodizer/schema.json)
[schemastore] catalog entry: ADD cfgd-config (vendor, src/schemas/json/cfgd-config.json)
```

## Full end-to-end example

```yaml
schemastore:
  repository:
    owner: tj-smith47              # your SchemaStore fork
    name: schemastore
    pull_request:
      enabled: true
      base:
        owner: SchemaStore
        name: schemastore
        branch: master
  commit_author:
    name: TJ Smith
    email: tj@jarvispro.io

  schemas:
    # EXTERNAL — anodizer's own .anodizer.yaml schema (#5727)
    - name: Anodizer
      file_match: [".anodizer.yaml", ".anodizer.yml"]
      url: "https://tj-smith47.github.io/anodizer/schema.json"
      description: "Anodizer Rust release-automation configuration file"

    # VENDOR — cfgd main config (draft-07)
    - name: cfgd-config
      slug: cfgd-config
      file_match: ["cfgd.yaml", ".cfgd.yaml"]
      schema_file: "schemas/cfgd-config.schema.json"
      crate: cfgd
      description: "cfgd machine configuration"

    # VENDOR — cfgd module (draft-2020-12: highSchemaVersion entry added automatically)
    - name: cfgd-module
      slug: cfgd-module
      file_match: ["**/modules/*.yaml"]
      schema_file: "schemas/cfgd-module.schema.json"
      crate: cfgd
      description: "cfgd module configuration"
      versioned: true
```

In this config:
- The **Anodizer** entry is external — SchemaStore gets only the catalog entry; no file changes on version bumps.
- The **cfgd-config** entry is vendored — the schema file is copied to `src/schemas/json/cfgd-config.json` on each release.
- The **cfgd-module** entry is vendored + versioned — emits `cfgd-module-<VER>.json` and merges the version into `versions`. Because it is draft-2020-12, anodizer automatically adds `cfgd-module` to the `highSchemaVersion` allowlist in the same PR.
- `repository` and `commit_author` are block-level defaults shared across all three entries; one PR carries all three changes.
