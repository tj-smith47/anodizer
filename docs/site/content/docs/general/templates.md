+++
title = "Templates"
description = "Tera template engine reference — variables, filters, and GoReleaser compatibility"
weight = 2
template = "docs.html"
+++

Anodize uses the [Tera](https://keats.github.io/tera/) template engine (Jinja2/Django-like syntax). Templates can be used in most string fields throughout the configuration: name templates, tag templates, message templates, signing arguments, and more.

## Syntax

Templates use `{{ "{{ }}" }}` for variable interpolation and `{{ "{% %}" }}` for control flow:

```yaml
name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
```

## GoReleaser compatibility

For easier migration from GoReleaser, anodize accepts Go-style templates with a leading dot. These are automatically preprocessed before rendering:

```yaml
# Both forms are equivalent:
name_template: "{{ .ProjectName }}-{{ .Version }}"   # Go-style (compat)
name_template: "{{ ProjectName }}-{{ Version }}"       # Tera-style (native)
```

You can freely mix both styles in the same config file. The leading dot is stripped before the template is rendered.

### Common GoReleaser idiom → Tera mapping

Anodize preprocesses most Go-template constructs into their Tera equivalents, but a handful of idioms copied verbatim from a `.goreleaser.yaml` will produce confusing errors. Use this table when migrating.

| GoReleaser idiom | Tera equivalent | Notes |
|---|---|---|
| `{{ if .IsRelease }}X{{ end }}` | `{% if IsRelease %}X{% endif %}` | Statement tags use `{% %}`, not `{{ }}` |
| `{{ if .IsRelease }}X{{ else }}Y{{ end }}` | `{% if IsRelease %}X{% else %}Y{% endif %}` | `{% else %}` |
| `{{ range .Tags }}...{{ end }}` | `{% for t in Tags %}...{% endfor %}` | Tera names the loop variable explicitly |
| `{{ range $k, $v := .Env }}...{{ end }}` | `{% for k, v in Env %}...{% endfor %}` | Key/value loop |
| `{{ with .Arm }}v{{ . }}{{ end }}` | `{% if Arm %}v{{ Arm }}{% endif %}` | Tera has no `with`; reference the field by name |
| `{{ tolower .Os }}` | `{{ Os \| lower }}` — or `{{ Os \| tolower }}` | Filters use `\|`; `tolower`/`toupper` aliases provided for parity |
| `{{ replace .Tag "v" "" }}` | `{{ Tag \| replace(from="v", to="") }}` | Tera filters take named args |
| `{{ trimprefix .Tag "v" }}` | `{{ Tag \| trimprefix(prefix="v") }}` | Alias filter registered for parity |
| `{{ .Env.FOO }}` | `{{ Env.FOO }}` — or `{{ .Env.FOO }}` | Dot-prefix form is preprocessed away |
| `{{ default "x" .Tag }}` | `{{ Tag \| default(value="x") }}` | Tera pipes the value through a filter |
| `{{ eq .Os "linux" }}` | `{% if Os == "linux" %}...{% endif %}` | Equality is a normal operator, not a function |
| `{{ printf "%s-%s" .Os .Arch }}` | `{{ Os }}-{{ Arch }}` | Most `printf` formats can be inlined; use filters for padding/number formatting |

If you hit a construct not covered here, open an issue with the failing template and the intended output.

## Template variables

### Project and version

| Variable | Description | Example |
|----------|-------------|---------|
| `ProjectName` | Project name from config | `myapp` |
| `Version` | Semantic version (without `v` prefix) | `1.2.3` |
| `RawVersion` | Version string as-is from Cargo.toml | `1.2.3-rc.1` |
| `Tag` | Full git tag | `v1.2.3` |
| `Major` | Major version component | `1` |
| `Minor` | Minor version component | `2` |
| `Patch` | Patch version component | `3` |
| `Prerelease` | Prerelease suffix (empty if none) | `rc.1` |

### Git information

| Variable | Description | Example |
|----------|-------------|---------|
| `FullCommit` | Full commit hash | `abc123def456...` |
| `ShortCommit` | Short commit hash | `abc1234` |
| `Commit` | Alias for `FullCommit` | `abc123def456...` |
| `Branch` | Current git branch name | `main` |
| `CommitDate` | ISO 8601 author date of HEAD | `2024-01-15T10:30:00Z` |
| `CommitTimestamp` | Unix timestamp of HEAD | `1705312200` |
| `PreviousTag` | Previous matching git tag | `v1.2.2` |
| `IsGitDirty` | `true` if working tree is dirty | `true` |
| `GitTreeState` | Working tree state | `clean` or `dirty` |

### Build context

| Variable | Description | Example |
|----------|-------------|---------|
| `Os` | Mapped OS name | `linux`, `darwin`, `windows` |
| `Arch` | Mapped architecture | `amd64`, `arm64` |
| `Target` | Full target triple | `x86_64-unknown-linux-gnu` |
| `Binary` | Current binary name | `myapp` |
| `ArtifactName` | Current artifact name | `myapp-1.0.0-linux-amd64.tar.gz` |
| `ArtifactPath` | Full path to artifact | `/path/to/dist/myapp-1.0.0.tar.gz` |
| `ArtifactExt` | Artifact extension (compound-aware) | `.tar.gz`, `.exe`, `.deb` |
| `Checksums` | Combined checksum file contents | `abc123  myapp.tar.gz\n...` |

### Release state

| Variable | Description | Example |
|----------|-------------|---------|
| `IsSnapshot` | `true` in snapshot mode | `true` |
| `IsDraft` | `true` if draft release | `false` |
| `IsNightly` | `true` in nightly mode | `false` |
| `ReleaseURL` | URL of created GitHub release | `https://github.com/...` |

### Time

| Variable | Description | Example |
|----------|-------------|---------|
| `Date` | Current date | `2024-01-15` |
| `Timestamp` | Current Unix timestamp | `1705312200` |
| `Now` | Current UTC time (ISO 8601) | `2024-01-15T10:30:00Z` |

### Environment variables

Access environment variables via `Env`:

```yaml
name_template: "{{ ProjectName }}-{{ Env.CUSTOM_SUFFIX }}"
```

You can define custom environment variables in the config:

```yaml
env:
  CUSTOM_SUFFIX: "special"
  BUILD_MODE: "production"
```

### Pipeline outputs

Stages can write values to the `Outputs` map, and templates can read them:

```yaml
# Tera-style
body_template: "Build ID: {{ Outputs.build_id }}"
# Go-style (also supported)
body_template: "Build ID: {{ .Outputs.build_id }}"
```

Similar to `Var.*` but for pipeline outputs rather than user config values.

> **Note:** Only reference keys that are actually set by stages. For optional keys, use the `| default` guard:
> ```yaml
> body_template: "Build: {{ Outputs.build_id | default(value=\"unknown\") }}"
> ```

## Filters

Tera provides many [built-in filters](https://keats.github.io/tera/docs/#built-in-filters). Common ones:

| Filter | Example | Result |
|--------|---------|--------|
| `lower` | `{{ "HELLO" \| lower }}` | `hello` |
| `upper` | `{{ "hello" \| upper }}` | `HELLO` |
| `title` | `{{ "hello world" \| title }}` | `Hello World` |
| `trim` | `{{ " hello " \| trim }}` | `hello` |
| `replace` | `{{ Version \| replace(from=".", to="_") }}` | `1_2_3` |
| `default` | `{{ Branch \| default(value="main") }}` | `main` |

### GoReleaser-compatible aliases

| Alias | Tera equivalent |
|-------|----------------|
| `tolower` | `lower` |
| `toupper` | `upper` |

### Custom filters

| Filter | Description | Example |
|--------|-------------|---------|
| `trimprefix` | Remove prefix | `{{ Tag \| trimprefix(prefix="v") }}` → `1.2.3` |
| `trimsuffix` | Remove suffix | `{{ File \| trimsuffix(suffix=".tar.gz") }}` |

## Control flow

Tera supports conditionals and loops:

```yaml
header: |
  {% if IsSnapshot %}
  **This is a snapshot build — not for production use.**
  {% else %}
  ## {{ ProjectName }} {{ Version }}
  {% endif %}
```

```yaml
# Loops (less common in config, but available)
message_template: |
  New release: {{ Tag }}
  {% for crate in crates %}
  - {{ crate.name }}: {{ crate.version }}
  {% endfor %}
```
