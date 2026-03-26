# Template Reference

Anodize uses the [Tera](https://keats.github.io/tera/) template engine (Jinja2/Django-like syntax). Templates can be used in most string fields throughout the configuration: name templates, tag templates, message templates, signing arguments, and more.

## Syntax

Templates use `{{ }}` for variable interpolation and `{% %}` for control flow:

```yaml
name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
```

### GoReleaser Compatibility

For easier migration from GoReleaser, Anodize also accepts Go-style templates with a leading dot. These are automatically preprocessed before rendering:

```yaml
# Both forms are equivalent:
name_template: "{{ .ProjectName }}-{{ .Version }}"   # Go-style (compat)
name_template: "{{ ProjectName }}-{{ Version }}"       # Tera-style (native)
```

You can freely mix both styles in the same config file. The leading dot is stripped before the template is rendered.

## Template Variables

### Project and Version

| Variable | Description | Example |
|----------|-------------|---------|
| `ProjectName` | Project name from config `project_name` field | `myapp` |
| `Version` | Semantic version (without `v` prefix) | `1.2.3` |
| `RawVersion` | Version string as-is from Cargo.toml (may include pre-release) | `1.2.3-rc.1` |
| `Tag` | Full git tag | `v1.2.3` |
| `Major` | Major version component | `1` |
| `Minor` | Minor version component | `2` |
| `Patch` | Patch version component | `3` |
| `Prerelease` | Prerelease suffix (empty if none) | `rc.1` |

### Git Information

| Variable | Description | Example |
|----------|-------------|---------|
| `FullCommit` | Full commit hash | `abc123def456...` |
| `ShortCommit` | Short commit hash | `abc1234` |
| `Commit` | Alias for `FullCommit` | `abc123def456...` |
| `Branch` | Current git branch name | `main` |
| `CommitDate` | ISO 8601 author date of HEAD commit | `2024-01-15T10:30:00Z` |
| `CommitTimestamp` | Unix timestamp of HEAD commit | `1705312200` |
| `PreviousTag` | Previous matching git tag (empty if none) | `v1.2.2` |
| `IsGitDirty` | `true` if working tree has uncommitted changes | `true` |
| `GitTreeState` | Working tree state | `clean` or `dirty` |

### Build Context

| Variable | Description | Example |
|----------|-------------|---------|
| `Os` | Mapped OS name (from target triple) | `linux`, `darwin`, `windows` |
| `Arch` | Mapped architecture (from target triple) | `amd64`, `arm64` |
| `Arm` | ARM version (if applicable) | `7` |
| `Binary` | Name of the current binary being archived | `myapp` |
| `ArtifactName` | Name of the current artifact | `myapp-1.0.0-linux-amd64.tar.gz` |
| `ArtifactPath` | Full path to the current artifact | `/path/to/dist/myapp-1.0.0.tar.gz` |

### Release State

| Variable | Description | Example |
|----------|-------------|---------|
| `IsSnapshot` | `true` if running in snapshot mode | `true` |
| `IsDraft` | `true` if release is a draft | `false` |
| `ReleaseURL` | URL of the created GitHub release | `https://github.com/...` |

### Time

| Variable | Description | Example |
|----------|-------------|---------|
| `Date` | Current date | `2024-01-15` |
| `Timestamp` | Current Unix timestamp | `1705312200` |
| `Now` | Current UTC time as ISO 8601 | `2024-01-15T10:30:00Z` |

### Environment Variables

| Variable | Description | Example |
|----------|-------------|---------|
| `Env.VAR` | Any environment variable | `{{ Env.GITHUB_TOKEN }}` |

Environment variables can be accessed via the `Env` namespace. This is commonly used for tokens, signing keys, and webhook URLs:

```yaml
sign:
  cmd: gpg
  args:
    - "--local-user"
    - "{{ Env.GPG_FINGERPRINT }}"
```

### Sign-stage Variables

These variables are only available within the `signs[]` configuration:

| Variable | Description |
|----------|-------------|
| `Signature` | Output path for the signature file |
| `Artifact` | Input path of the artifact being signed |

## Filters

### Built-in Tera Filters

Tera provides many built-in filters. Here are the most commonly used ones:

| Filter | Usage | Description |
|--------|-------|-------------|
| `lower` | `{{ Os \| lower }}` | Convert to lowercase |
| `upper` | `{{ Os \| upper }}` | Convert to uppercase |
| `trim` | `{{ value \| trim }}` | Strip leading/trailing whitespace |
| `title` | `{{ value \| title }}` | Capitalize first letter of each word |
| `replace` | `{{ value \| replace(from="old", to="new") }}` | Replace substrings |
| `default` | `{{ value \| default(value="fallback") }}` | Provide a default for undefined variables |
| `length` | `{{ list \| length }}` | Get string/array length |
| `truncate` | `{{ value \| truncate(length=20) }}` | Truncate string to length |
| `first` | `{{ list \| first }}` | Get first element |
| `last` | `{{ list \| last }}` | Get last element |
| `join` | `{{ list \| join(sep=", ") }}` | Join array elements |

See the [Tera documentation](https://keats.github.io/tera/docs/#built-in-filters) for the complete list.

### Custom Filters (GoReleaser-compatible)

Anodize registers the following custom filters for compatibility with GoReleaser templates:

| Filter | Usage | Description |
|--------|-------|-------------|
| `tolower` | `{{ "FOO" \| tolower }}` | Convert to lowercase (alias for `lower`) |
| `toupper` | `{{ "foo" \| toupper }}` | Convert to uppercase (alias for `upper`) |
| `trimprefix` | `{{ Tag \| trimprefix(prefix="v") }}` | Strip a prefix from a string |
| `trimsuffix` | `{{ Name \| trimsuffix(suffix=".exe") }}` | Strip a suffix from a string |

**Filter chaining:** Filters can be chained with the pipe (`|`) operator:

```yaml
name_template: "{{ Tag | trimprefix(prefix='v') | upper }}"
# v1.2.3 -> 1.2.3 -> 1.2.3 (upper has no effect on digits)
```

## Control Flow

### Conditionals

Use `{% if %}`, `{% elif %}`, `{% else %}`, and `{% endif %}` for conditional rendering:

```yaml
name_template: "{% if IsSnapshot %}SNAPSHOT-{% endif %}{{ ProjectName }}-{{ Version }}"
```

Boolean template variables (`IsSnapshot`, `IsDraft`, `IsGitDirty`) are stored as real booleans, so they work directly in conditionals.

More complex example:

```yaml
name_template: >
  {% if IsSnapshot %}{{ ProjectName }}-SNAPSHOT-{{ ShortCommit }}
  {% elif Prerelease %}{{ ProjectName }}-{{ Version }}-pre
  {% else %}{{ ProjectName }}-{{ Version }}{% endif %}
```

### Loops

Use `{% for %}` and `{% endfor %}` for iteration:

```
{% for item in list %}
  - {{ item }}
{% endfor %}
```

Note: loops are rarely needed in release configuration but are available for advanced use cases.

## Examples

### Archive naming with OS-specific behavior

```yaml
archives:
  - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
```

This produces filenames like `myapp-1.2.3-linux-amd64.tar.gz`.

### Snapshot versioning

```yaml
snapshot:
  name_template: "{{ Version }}-SNAPSHOT-{{ ShortCommit }}"
```

Produces versions like `1.2.3-SNAPSHOT-abc1234` for snapshot builds.

### Tag template with prefix stripping

```yaml
tag_template: "v{{ Version }}"
```

When used in reverse (extracting version from tag):

```yaml
name_template: "{{ Tag | trimprefix(prefix='v') }}"
```

### Conditional snapshot naming

```yaml
name_template: "{% if IsSnapshot %}{{ ProjectName }}-dev-{{ ShortCommit }}{% else %}{{ ProjectName }}-{{ Version }}{% endif %}-{{ Os }}-{{ Arch }}"
```

### Docker image tags

```yaml
docker:
  - image_templates:
      - "ghcr.io/{{ Env.GITHUB_REPOSITORY }}:{{ Version }}"
      - "ghcr.io/{{ Env.GITHUB_REPOSITORY }}:{{ Major }}.{{ Minor }}"
      - "ghcr.io/{{ Env.GITHUB_REPOSITORY }}:latest"
```

### Signing arguments with environment variables

```yaml
signs:
  - cmd: gpg
    args:
      - "--batch"
      - "--local-user"
      - "{{ Env.GPG_FINGERPRINT }}"
      - "--output"
      - "{{ Signature }}"
      - "--detach-sig"
      - "{{ Artifact }}"
```

### Release notes with header/footer

```yaml
release:
  header: |
    ## {{ ProjectName }} {{ Version }}
    Released on {{ Date }}
  footer: |
    **Full Changelog**: https://github.com/myorg/myapp/compare/{{ PreviousTag }}...{{ Tag }}
```

### Announcement templates

```yaml
announce:
  slack:
    enabled: true
    webhook_url: "{{ Env.SLACK_WEBHOOK_URL }}"
    message_template: "{{ ProjectName }} {{ Tag }} has been released! {{ ReleaseURL }}"
```

## Error Handling

If a template contains a syntax error or references an undefined variable, anodize will report the error with the original template string for easier debugging. Common errors include:

- **Undefined variable**: `{{ Nonexistent }}` will fail with an error naming the variable.
- **Missing filter argument**: `{{ Tag | trimprefix }}` fails because `trimprefix` requires a `prefix` argument.
- **Unclosed blocks**: `{% if Var %}text` without `{% endif %}` will fail.
- **Unknown filters**: `{{ Tag | nonexistent_filter }}` will fail with an error naming the filter.

Use the `default` filter to handle potentially undefined variables gracefully:

```yaml
name_template: "{{ Undefined | default(value='fallback') }}"
```
