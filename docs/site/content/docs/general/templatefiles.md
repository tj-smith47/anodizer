+++
title = "Template Files"
description = "Render template files through the template engine and include them in releases"
weight = 10
template = "docs.html"
+++

The `template_files` stage renders source files through the template engine and automatically uploads the output as release assets.

## Minimal config

```yaml
template_files:
  - src: install.sh.tpl
    dst: install.sh
```

## Template files config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | `default` | Identifier for this template file entry |
| `src` | string | — | Source template file path (supports templates) |
| `dst` | string | — | Destination filename within the dist directory (supports templates) |
| `mode` | string | `0655` | File permissions in octal notation (Unix only) |

## Behavior

- Both `src` and `dst` paths are rendered through the template engine before use
- The source file contents are also rendered through the template engine
- Output files are written to `dist/<dst>`
- Each output is registered as an uploadable artifact (included in releases, checksums, and signing)
- Path traversal (`..`) and absolute paths in `dst` are rejected for security
- The stage can be skipped with `--skip templatefiles`

## Template rendering

Use any template variable in both the file paths and file contents:

```yaml
template_files:
  - id: install-script
    src: "scripts/{{ ProjectName }}-install.sh.tpl"
    dst: "{{ ProjectName }}-{{ Version }}-install.sh"
    mode: "0755"
```

Given a source file `scripts/myapp-install.sh.tpl`:

```bash
#!/bin/sh
# Install {{ ProjectName }} {{ Version }}
curl -L https://github.com/myorg/{{ ProjectName }}/releases/download/{{ Tag }}/{{ ProjectName }}-{{ Os }}-{{ Arch }}.tar.gz | tar xz
```

## Setting file permissions

Use the `mode` field to set executable permissions on generated scripts:

```yaml
template_files:
  - src: run.sh.tpl
    dst: run.sh
    mode: "0755"
```

The mode must be a string in octal notation (e.g., `"0755"`, `"0644"`). The default is `0655`.

## Multiple template files

```yaml
template_files:
  - id: install-script
    src: install.sh.tpl
    dst: install.sh
    mode: "0755"
  - id: config-example
    src: config.yaml.tpl
    dst: config.example.yaml
  - id: completion
    src: completions/bash.tpl
    dst: "{{ ProjectName }}.bash"
```

Each entry gets its own artifact ID, so you can reference them individually in publisher configs.
