+++
title = "Project Name"
description = "Configure the project name used across all stages"
weight = 1
template = "docs.html"
+++

The `project_name` field sets the name used in archive templates, release names, and anywhere `{{ ProjectName }}` appears in a template.

## Minimal config

```yaml
project_name: myapp
```

## Behavior

- Used as the default value for `{{ ProjectName }}` in templates
- If omitted, defaults to an empty string
- Does **not** need to match your `Cargo.toml` package name (though it usually should)

## Dist directory

The `dist` field controls where build artifacts are placed:

```yaml
project_name: myapp
dist: ./dist       # default
```

All compiled binaries, archives, checksums, and other artifacts are written to this directory. It's created automatically if it doesn't exist. Use `--clean` to remove it before a release.
