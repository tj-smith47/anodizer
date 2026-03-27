+++
title = "Archives"
description = "Package binaries into tar.gz, zip, tar.xz, or tar.zst archives"
weight = 1
template = "docs.html"
+++

The archive stage packages your compiled binaries into distributable archives.

## Minimal config

```yaml
crates:
  - name: myapp
    archives:
      - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
```

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
