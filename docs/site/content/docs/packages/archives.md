+++
title = "Archives"
description = "Package binaries into tar.gz, zip, tar.xz, or tar.zst archives"
weight = 1
template = "docs.html"
+++

The archive stage packages your compiled binaries into distributable archives.

## Classification

Packager — builds distributable archives from compiled binaries. Required: not a publisher; always runs unless disabled.

## Minimal config

```yaml
crates:
  - name: myapp
    archives:
      - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
```

## Full config reference

```yaml
crates:
  - name: myapp
    archives:
      - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"  # optional
        format: tar.gz                  # optional; tar.gz | tar.xz | tar.zst | zip | binary
        format_overrides:               # optional; per-OS format overrides
          - os: windows
            format: zip
        files: []                       # optional; extra files to include
        binaries: []                    # optional; specific binaries (default: all)
        wrap_in_directory: ""           # optional; wrap contents in a subdirectory
```

## Authentication

Not applicable — archive generation is a local build step with no external service calls.

## Common gotchas

- **Format overrides**: `format_overrides` is matched by OS name (`linux`, `darwin`, `windows`). An unmatched override is silently ignored.
- **`wrap_in_directory`**: wrapping in a subdirectory changes the extraction path. Consumers expecting a flat archive will need to adjust their install scripts.
- **`archives: false`**: disables archiving entirely; binaries are distributed as raw files.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Archive config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name_template` | string | `{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}` | Archive filename (without extension) |
| `format` | string | `tar.gz` | Archive format: `tar.gz`, `tar.xz`, `tar.zst`, `zip`, `binary` |
| `format_overrides` | list | none | Per-OS format overrides |
| `files` | list | none | Extra files to include (e.g., `LICENSE`, `README.md`) |
| `binaries` | list | all | Specific binaries to include (default: all from builds) |
| `wrap_in_directory` | string | none | Wrap contents in a subdirectory |

## Format overrides

Use different formats for different operating systems:

```yaml
archives:
  - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
    format: tar.gz
    format_overrides:
      - os: windows
        format: zip
```

## Including extra files

```yaml
archives:
  - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
    files:
      - LICENSE
      - README.md
      - config.example.yaml
```

## Shell completions & man pages

Auto-generate (or harvest, or copy) shell completions and man pages and bundle
them into every archive. Three mutually-exclusive modes per block — set exactly
one of `generate` / `from_build_out` / `copy`:

```yaml
archives:
  - id: default
    completions:
      # Mode A — run the host-native binary once per shell, reuse for all targets.
      generate: "{{ ArtifactPath }} completions {{ Shell }}"
      shells: [bash, zsh, fish, powershell, nushell, elvish]   # arbitrary list
      dst: "completions/"
      # Mode B — harvest a build.rs OUT_DIR (clap_complete) via a per-target glob:
      #   from_build_out: "**/out/{{ Binary }}.{bash,fish,zsh}"
      # Mode C — copy committed files:
      #   copy: "contrib/completion/*"
    manpages:
      generate: "{{ ArtifactPath }} --man"     # or from_build_out / copy
      dst: "man/man1/"
```

Mode A generates **once on the host-native target** (completions/man pages do
not vary by architecture) and reuses the output for every target's archive. A
pure cross build with no host-native artifact errors clearly — use
`from_build_out` or `copy` instead, or add the host target.

Per-shell filenames follow the clap_complete convention so files drop straight
into the shell's lookup path: bash `<bin>`, zsh `_<bin>`, fish `<bin>.fish`,
powershell `_<bin>.ps1`, elvish `<bin>.elv`, nushell `<bin>.nu`. Man pages are
written as `<bin>.1`.

In `from_build_out` / `copy` globs, `{{ Binary }}` resolves to the
host-native binary's name — but on a pure cross build (no host artifact) it
falls back to the **crate name**. If your binary name differs from the crate
name, spell it literally in the glob instead of relying on `{{ Binary }}`.

### Single source of truth for nfpm

Generated files are staged under the dist directory so the **same** files feed
both the archive and any nfpm package — generate once, ship everywhere:

```text
dist/.completions/<crate>/   # e.g. dist/.completions/rg/rg.fish
dist/.manpages/<crate>/      # e.g. dist/.manpages/rg/rg.1
```

Point an nfpm `contents:` entry at the staging dir to install them system-wide:

```yaml
nfpm:
  - contents:
      - src: "dist/.completions/rg/*"
        dst: /usr/share/bash-completion/completions/
      - src: "dist/.manpages/rg/*"
        dst: /usr/share/man/man1/
```

## Raw binary (no archive)

Use `format: binary` to skip archiving and distribute the raw binary:

```yaml
archives:
  - format: binary
    name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
```

## Disabling archives

```yaml
crates:
  - name: myapp
    archives: false    # skip archiving entirely
```

## Full example

```yaml
crates:
  - name: myapp
    archives:
      - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
        format: tar.gz
        format_overrides:
          - os: windows
            format: zip
        files: [LICENSE, README.md]
        wrap_in_directory: "{{ ProjectName }}-{{ Version }}"
```
