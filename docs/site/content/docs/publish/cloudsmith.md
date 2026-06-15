+++
title = "Cloudsmith"
description = "Upload packages to Cloudsmith repositories"
weight = 82
template = "docs.html"
+++

Anodizer can upload deb, rpm, and apk packages to [Cloudsmith](https://cloudsmith.io/) repositories.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Assets | false | structured warn line per (org, repo, filename) tuple (DELETE migration pending) | `CLOUDSMITH_API_KEY package_delete` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## The `required:` field

Default: **`false`** — a Cloudsmith upload failure is logged but does not fail the release.

Set `required: true` to make the release exit non-zero if this publisher fails:

```yaml
cloudsmiths:
  - organization: myorg
    repository: myrepo
    required: true
```

See [Publish overview — the `required:` field](../) for the full semantics.

## Minimal config

```yaml
cloudsmiths:
  - organization: myorg
    repository: myrepo
```

## Full config reference

```yaml
cloudsmiths:
  - organization: myorg          # required
    repository: releases         # required
    formats:                     # default: [apk, deb, rpm]
      - deb
      - rpm
    distributions:               # per-format distribution tag
      deb: "ubuntu/jammy"
      rpm: "el/8"
      alpine: "alpine/any-version"
    component: main              # deb only
    secret_name: CLOUDSMITH_TOKEN
    republish: true              # allow overwriting existing versions
    keep_versions: 3             # keep 3 newest releases, prune older (opt-in)
    ids: []                      # filter by build IDs
    skip: false                  # skip this config
```

## Authentication

| Variable | Description |
|----------|-------------|
| `CLOUDSMITH_TOKEN` | Cloudsmith API key (or custom name via `secret_name`) |

## Common gotchas

- If `distributions` is omitted, packages are uploaded without a distribution tag; some Cloudsmith repo configurations require a valid distribution to index the package.
- The `component` field only affects deb packages. Setting it for rpm or apk has no effect.
- Format detection is by file extension: `.apk` maps to `alpine` (not `apk`) in the config.

## Republish / update behavior

When `republish: true`, anodizer opts into the Cloudsmith API's explicit replace-prior-version path, preventing MD5 conflicts when re-cutting a version. See [Recovery flags: cloudsmith.republish](../advanced/recovery-flags.md#cloudsmith-republish) for the full mechanism.

## Retention: `keep_versions` {#retention-keep_versions}

`keep_versions: N` retains only the `N` most-recent **release** versions of each published package, pruning older ones from the repository after a successful upload. It is the durable remedy for storage-capped repositories — notably the Cloudsmith free plan's 500&nbsp;MB limit, which offers no server-side retention policy.

```yaml
cloudsmiths:
  - organization: myorg
    repository: releases
    keep_versions: 3   # keep the 3 newest releases, prune anything older
```

Behavior:

- **Opt-in and destructive.** Leaving `keep_versions` unset (the default) prunes nothing. `keep_versions: 0` is rejected — anodizer never prunes every version.
- **Per package.** After upload, anodizer lists every version of *this* package, ranks the distinct releases by SemVer (newest first), keeps the top `N` — always including the version just published — and deletes every artifact (all formats and architectures) of versions ranked beyond `N`. Other packages sharing the repository are untouched.
- **Format-aware.** The deb/rpm epoch (`1:0.9.1-1`) and apk revision (`0.9.1-r1`) suffixes are normalized to the base SemVer (`0.9.1`), so keeping `2` versions keeps every `.deb`/`.rpm`/`.apk` of the two newest releases — not two formats of one release.
- **Best-effort, non-fatal.** Pruning runs only after the upload (the real work) has already succeeded, is skipped in dry-run and snapshot mode, and a list/delete failure emits a prominent warning and continues — it never fails the release or rolls anything back.

The per-package summary appears at default verbosity (`pruned M old artifact(s) … (kept N most-recent: 0.9.1, 0.9.0, …)`); per-artifact `DELETE` detail is shown under `-v`.

## Cloudsmith config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `organization` | string | **required** | Cloudsmith organization name (template) |
| `repository` | string | **required** | Cloudsmith repository name (template) |
| `ids` | list | none | Filter by build IDs |
| `formats` | list | `["apk", "deb", "rpm"]` | Package format filter |
| `distributions` | map | none | Distribution mapping per format (e.g., `deb: "ubuntu/focal"`) |
| `component` | string | none | Debian component name (e.g., `"main"`) |
| `secret_name` | string | `CLOUDSMITH_TOKEN` | Environment variable name for the API key |
| `skip` | string/bool | none | Skip this config |
| `republish` | string/bool | `false` | Allow overwriting existing package versions. See [Recovery flags](../advanced/recovery-flags.md#cloudsmith-republish). |
| `keep_versions` | integer | none | Retain only the `N` newest releases per package, pruning older ones after upload (opt-in, destructive, best-effort). See [Retention](#retention-keep_versions). |

## Format detection

Packages are matched by file extension:

| Extension | Format |
|-----------|--------|
| `.deb` | `deb` |
| `.rpm` | `rpm` |
| `.apk` | `alpine` |
| other | `raw` |

## Distribution mapping

Map package formats to specific distributions:

```yaml
cloudsmiths:
  - organization: myorg
    repository: myrepo
    distributions:
      deb: "ubuntu/focal"
      rpm: "el/8"
    component: main
```

## Full example

```yaml
cloudsmiths:
  - organization: myorg
    repository: releases
    formats:
      - deb
      - rpm
    distributions:
      deb: "ubuntu/jammy"
    component: main
    republish: true
```
