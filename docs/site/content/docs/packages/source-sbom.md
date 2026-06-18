+++
title = "Source Archives & SBOM"
description = "Generate source archives and software bill of materials"
weight = 5
template = "docs.html"
+++

## Classification

Packager — generates source archives and software bill of materials (SBOM) files from the repository. Required: not a publisher; both stages are disabled by default.

## Minimal config

Both stages are opt-in. Enable each independently:

```yaml
source:
  enabled: true        # tar.gz of git-tracked files

sbom:
  enabled: true        # built-in CycloneDX from Cargo.lock
```

## Full config reference

### Source archive

```yaml
source:
  enabled: false                     # required; opt-in (disabled by default)
  format: tar.gz                     # optional; tar.gz | tgz | tar | zip
  name_template: ""                  # optional; archive filename without extension (template)
  prefix_template: ""                # optional; directory prefix inside archive (template)
  files: []                          # optional; extra files beyond git-tracked files
```

### SBOM generation

```yaml
sbom:
  enabled: false                     # required; opt-in (disabled by default)
  id: default                        # optional; unique identifier
  cmd: ""                            # optional; external command (e.g. syft); omit for built-in
  args: []                           # optional; command-line arguments
  env: {}                            # optional; environment variables for the command
  documents: []                      # optional; output document path templates
  artifacts: archive                 # optional; source | archive | binary | package | diskimage | installer | any
  ids: []                            # optional; filter by artifact IDs
  disable: false                     # optional; bool or template string
```

---

## Authentication

Not applicable — source archive and SBOM generation are local build steps with no external service calls.

## Common gotchas

- **Source archives**: extra `files` beyond git-tracked files must exist at the path specified (after template rendering). Missing files cause a build error.
- **SBOM built-in mode**: requires `Cargo.lock` to be present and up-to-date. If the lock file is absent, anodizer errors.
- **SBOM external mode**: the external command (e.g., `syft`) must be on `PATH`. Anodizer does not install it.
- **SDE compliance**: built-in CycloneDX/SPDX output embeds `SOURCE_DATE_EPOCH` as the document timestamp, making the SBOM byte-stable across determinism runs.

## Republish / update behavior

Not applicable — these are local packaging stages, not publishers.

## Source archives

The source stage creates a distributable archive of your repository's tracked source files using `git archive`. Only files tracked by git are included (untracked and gitignored files are automatically excluded). The resulting archive is registered as a release artifact.

### Minimal config

```yaml
source:
  enabled: true
```

This produces a `tar.gz` archive named `<project>-<version>.tar.gz` containing all git-tracked files.

### Source config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable source archive generation |
| `format` | string | `tar.gz` | Archive format: `tar.gz`, `tgz`, `tar`, or `zip` |
| `name_template` | string | `{{ ProjectName }}-{{ Version }}` | Archive filename without extension (supports templates) |
| `prefix_template` | string | none (no prefix) | Directory prefix inside the archive (supports templates) |
| `files` | list | none | Extra files to include beyond git-tracked files |

### How it works

1. Anodizer runs `git archive` against the current commit (or HEAD) to create an archive of all tracked files.
2. If `prefix_template` is set, all paths inside the archive are nested under that directory (e.g., `myapp-1.0.0/src/main.rs`).
3. If extra `files` are specified, they are appended to the tar archive under the same prefix directory. For zip format, extra files are added via `git archive --add-file`.
4. For `tar.gz` format with extra files, the archive is built as an uncompressed tar first, extra files are appended, then the result is gzip-compressed.

### Extra files

The `files` list accepts simple glob strings or objects with `src`, `dst`, `strip_parent`, and `info` fields. File source paths are template-rendered before glob expansion.

**Simple strings:**

```yaml
source:
  enabled: true
  files:
    - "LICENSE"
    - "README.md"
    - "crates/**/*.rs"
```

**Objects with destination mapping and metadata:**

```yaml
source:
  enabled: true
  files:
    - src: "scripts/install.sh"
      dst: "scripts"
      info:
        mode: 0o755
    - src: "config/default.toml"
      dst: "config/default.toml"
      strip_parent: true
      info:
        owner: root
        group: root
        mtime: "2025-01-01T00:00:00Z"
```

| File entry field | Type | Description |
|------------------|------|-------------|
| `src` | string | Source file path or glob pattern |
| `dst` | string | Destination path within the archive prefix directory |
| `strip_parent` | bool | Strip the parent directory from the source path |
| `info.owner` | string | File owner in the archive |
| `info.group` | string | File group in the archive |
| `info.mode` | int | File permission mode (octal) |
| `info.mtime` | string | Modification time (RFC 3339 format or unix timestamp) |

### Full source example

```yaml
source:
  enabled: true
  format: tar.gz
  name_template: "{{ ProjectName }}-{{ Version }}-source"
  prefix_template: "{{ ProjectName }}-{{ Version }}"
  files:
    - "Cargo.toml"
    - "Cargo.lock"
    - "LICENSE"
    - "README.md"
```

This produces `myapp-1.0.0-source.tar.gz` with all files nested under `myapp-1.0.0/`.

---

## SBOM generation

The SBOM (Software Bill of Materials) stage produces a machine-readable inventory of your project's dependencies. SBOMs are used for supply chain security, license compliance, and vulnerability tracking. The generated SBOM is attached as a release artifact.

Anodizer supports two modes:

1. **Built-in mode** -- parses your `Cargo.lock` file and generates a CycloneDX 1.5 or SPDX 2.3 JSON document. No external tools required.
2. **External command mode** -- runs an external cataloging tool (default: [syft](https://github.com/anchore/syft)) against your artifacts. This works with any language ecosystem.

### Minimal config (built-in)

```yaml
sbom:
  enabled: true
```

When no `cmd` or `args` are specified, anodizer uses built-in mode. It locates `Cargo.lock` by searching from the repository root upward, parses all package entries, and emits a CycloneDX 1.5 JSON file named `<project>-<version>.cdx.json`.

### SBOM config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | `"default"` | Unique identifier for this SBOM config |
| `cmd` | string | none (built-in mode) | External command to run (e.g., `syft`) |
| `args` | list | syft defaults | Command-line arguments (supports `$artifact`, `$document` variables) |
| `env` | map or list | syft defaults | Environment variables for the command |
| `documents` | list | auto | Output document path templates |
| `artifacts` | string | `"archive"` | Which artifact type to catalog: `source`, `archive`, `binary`, `package`, `diskimage`, `installer`, `any` |
| `ids` | list | none | Filter to only catalog artifacts matching these IDs |
| `disable` | bool or string | `false` | Disable this config (accepts template strings) |

### Built-in mode

Built-in mode requires no external tools. The output format is determined by the `documents` field:

- If any document path contains `spdx` (case-insensitive), SPDX 2.3 JSON is generated.
- Otherwise, CycloneDX 1.5 JSON is the default.

**CycloneDX output** (default):

The generated document follows the CycloneDX 1.5 specification. Each dependency from `Cargo.lock` becomes a component with:
- Package name and version
- A Package URL (`purl`) in `pkg:cargo/<name>@<version>` format
- An external reference to crates.io for registry dependencies

Output filename: `<project>-<version>.cdx.json`

**SPDX output:**

The generated document follows SPDX 2.3 with CC0-1.0 data license. It includes:
- A root package representing your project
- One SPDX package per dependency with `DEPENDS_ON` relationships
- Package URLs and download locations for registry dependencies

Output filename: `<project>-<version>.spdx.json`

To select SPDX format:

```yaml
sboms:
  - id: spdx
    documents:
      - "{{ ProjectName }}-{{ Version }}.spdx.json"
```

### External command mode

When `cmd` or `args` are specified, anodizer runs an external tool against each matching artifact. This is useful for non-Rust projects or when you need richer SBOM output.

The default external tool is [syft](https://github.com/anchore/syft). When using syft, anodizer provides sensible default arguments and environment variables automatically:

**Default syft args:**
```
syft $artifact --output spdx-json=$document --enrich all
```

**Default syft env** (for source/archive artifacts):
```
SYFT_FILE_METADATA_CATALOGER_ENABLED=true
```

#### Variable substitution in args

| Variable | Description |
|----------|-------------|
| `$artifact` | Path to the artifact being cataloged |
| `$document` | Path to the first output document |
| `$document0`, `$document1`, ... | Indexed output document paths |
| `$artifactID` | The artifact's ID metadata |

#### Template variables in documents

Document path templates have access to all standard template variables plus:

| Variable | Description |
|----------|-------------|
| `{{ ArtifactName }}` | Filename of the artifact being cataloged |
| `{{ ArtifactExt }}` | File extension of the artifact |
| `{{ ArtifactID }}` | ID metadata of the artifact |
| `{{ Os }}` | Target operating system |
| `{{ Arch }}` | Target architecture |

### External command example

```yaml
sboms:
  - id: archive-sbom
    cmd: syft
    artifacts: archive
    documents:
      - "{{ ArtifactName }}.sbom.json"
    args:
      - "$artifact"
      - "--output"
      - "spdx-json=$document"
    env:
      SYFT_FILE_METADATA_CATALOGER_ENABLED: "true"
```

### Multiple SBOM configs

You can define multiple SBOM configurations. Each must have a unique `id`:

```yaml
sboms:
  - id: builtin-cyclonedx
    documents:
      - "{{ ProjectName }}-{{ Version }}.cdx.json"
  - id: syft-archives
    cmd: syft
    artifacts: archive
    documents:
      - "{{ ArtifactName }}.sbom.json"
```

The config accepts both singular `sbom` (single object) and plural `sboms` (array) forms.

### Disabling an SBOM config

Skip with a boolean or a template expression:

```yaml
sboms:
  - id: default
    skip: true
  - id: conditional
    skip: "{{ if Env.SKIP_SBOM }}true{{ end }}"
```

---

## Pipeline placement

Both stages run after builds and archiving but before the release and publish stages:

```
build -> archive -> source -> sbom -> checksum -> sign -> release -> publish
```

Source archives and SBOMs are registered as release artifacts. They appear alongside your binaries and archives in the release, and are included in checksum and signing stages that follow.
