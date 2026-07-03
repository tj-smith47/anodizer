+++
title = "Templates"
description = "Tera template engine reference — variables, filters, and GoReleaser compatibility"
weight = 2
template = "docs.html"
+++

Anodizer uses the [Tera](https://keats.github.io/tera/) template engine (Jinja2/Django-like syntax). Templates can be used in most string fields throughout the configuration: name templates, tag templates, message templates, signing arguments, and more.

## Two syntaxes, one engine

Anodizer accepts **both** template dialects and renders them on the same Tera engine:

- **Tera-native, no-dot** — the canonical, recommended form. Reference fields by bare name (`{{ Version }}`), use Tera operators (`==`, `!=`, `and`, `or`, `not`), pipe through filters (`{{ Tag | trimprefix(prefix="v") }}`), and write control flow with `{% %}`.
- **GoReleaser / Go `text/template`** — paste a snippet straight out of a `.goreleaser.yaml` and it runs unchanged. Anodizer auto-translates Go idioms before rendering, so migrating costs nothing.

```yaml
# Both forms are equivalent — pick either, mix freely:
name_template: "{{ ProjectName }}-{{ Version }}"     # Tera-native (recommended)
name_template: "{{ .ProjectName }}-{{ .Version }}"   # GoReleaser/Go (auto-translated)
```

The docs throughout this site use the Tera-native no-dot form as the canonical idiom; the Go form is documented here for painless migration.

## Syntax

Templates use `{{ }}` for variable interpolation and `{% %}` for control flow:

```yaml
name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
```

## Undefined variables

Anodizer runs Tera in strict mode: referencing an undefined variable is a
render error by default, so a typo in a name template fails the build
instead of silently baking a blank into a release artifact.

- **Top-level access errors**, naming the missing variable:

  ```yaml
  name_template: "{{ Typo }}"
  ```

  ```text
  error: Variable `Typo` is not defined.
  ```

  The error also lists the variables actually available in that rendering
  context — Tera appends `Available variables: ...` with the live context
  keys. That list varies by stage (a build-stage render exposes `Os` /
  `Arch`; a top-level render adds 20+ git-derived variables) and isn't
  reproduced here since it isn't a fixed contract.

- **An undefined operand inside `~` string-concat renders as empty**, not an
  error — this is Tera's own coercion rule for the `~` operator, not
  something anodizer configures:

  ```yaml
  name_template: "{{ Typo ~ '-rc1' }}"
  # -> "-rc1"
  ```

- **`.Env.MISSING` (Go-style) renders empty by design** — env var
  references always resolve, defaulting to `""` instead of erroring, since
  most templates only reference an env var conditionally:

  ```yaml
  name_template: "{{ .Env.MISSING }}"
  # -> ""
  ```

For an *intentional* default rather than an accidental empty string, reach
for one of these idioms:

```yaml
# Top-level fallback
name_template: "{{ Typo or \"default\" }}"

# Optional chaining into a (possibly absent) nested field
name_template: "{{ Some?.Missing or \"\" }}"
```

`or` short-circuits past an undefined left-hand side the same way `~`
coerces one. `?.` suppresses the Undefined error at *every* link of a
dotted path — including a wholly undefined root — not just a missing leaf
field, so it composes with `or` for a safe default anywhere in a chain.

## GoReleaser compatibility

Anodizer auto-translates Go `text/template` syntax to its Tera equivalent before rendering, so a template copied verbatim from a `.goreleaser.yaml` works without edits. The translation covers:

- **Leading dots** — `{{ .Field }}` → `{{ Field }}` (and `{{ .Env.FOO }}` → `{{ Env.FOO }}`).
- **Go statement blocks** — `{{ if }}` / `{{ range }}` / `{{ with }}` / `{{ end }}` become Tera's `{% if %}` / `{% for %}` / `{% endif %}` / `{% endfor %}`.
- **`$` variables** — `$myvar` Go locals are accepted.
- **Comparison & logic functions** — `eq` `ne` `gt` `lt` `ge` `le` `and` `or` `not` map to Tera operators (`==` `!=` `>` `<` `>=` `<=` `and` `or` `not`).
- **`len`** — `{{ len .Tags }}` becomes `{{ Tags | length }}`.
- **Positional function calls** — Go-style positional arguments for `replace` `split` `contains` `in` `reReplaceAll` `map` `slice` `time` `printf` `print` `println` are mapped to Tera's named-argument form.
- **tera 1.x numeric indexing** — `list.0` / `a.0.b` / `a?.0` rewrite to the native `list[0]` / `a[0].b` / `a?[0]`. Numeric segments index arrays: a map key that is the string `"0"` needs `["0"]`, not `.0`. Write `[N]` in new templates.

```yaml
# Both forms are equivalent:
name_template: "{{ .ProjectName }}-{{ .Version }}"   # Go-style (compat)
name_template: "{{ ProjectName }}-{{ Version }}"       # Tera-style (native)
```

You can freely mix both styles in the same config file. The leading dot is stripped before the template is rendered.

### Common GoReleaser idiom → Tera mapping

Anodizer preprocesses most Go-template constructs into their Tera equivalents, but a handful of idioms copied verbatim from a `.goreleaser.yaml` will produce confusing errors. Use this table when migrating.

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
| `Arm` | 32-bit ARM version, set only where `Arch` is the bare `arm` (archive asset names split `armv7` into `Arch="arm"` + `Arm="7"`); empty everywhere `Arch` carries the composite `armv7`/`armv6` token (build, makeself, AppImage) | `7` |
| `Arm64` | 64-bit ARM feature level (build/installer contexts) | `v8` |
| `Amd64` | x86-64 micro-architecture level from the binary's build metadata; untagged binaries carry the `v1` baseline in every context. Default name templates suppress `v1` (`{% if Amd64 and Amd64 != "v1" %}`), so only tuned `v2`/`v3` builds get a suffix | `v1`, `v3` |
| `Mips` | Always empty — `Arch` carries the full mips token (`mips64el`), so a suffix would double it | (empty) |
| `I386` | 32-bit x86 instruction floor (build/installer contexts) | `sse2` |
| `Target` | Full target triple | `x86_64-unknown-linux-gnu` |
| `Binary` | Current binary name | `myapp` |
| `ArtifactName` | Current artifact name | `myapp-1.0.0-linux-amd64.tar.gz` |
| `ArtifactPath` | Full path to artifact | `/path/to/dist/myapp-1.0.0.tar.gz` |
| `ArtifactExt` | Artifact extension (compound-aware) | `.tar.gz`, `.exe`, `.deb` |
| `Checksums` | Combined checksum file contents | `abc123  myapp.tar.gz\n...` |
| `SourcePrefix` | Top-level directory inside the source archive (from a `source.prefix_template` ending in `/`); empty for a flat archive. Set by the source stage; useful for an SRPM `%autosetup -n {{ SourcePrefix }}`. | `myapp-1.2.3` |

### Release state

| Variable | Description | Example |
|----------|-------------|---------|
| `IsSnapshot` | `true` in snapshot mode | `true` |
| `IsDraft` | `true` if draft release | `false` |
| `IsNightly` | `true` in nightly mode | `false` |
| `ReleaseURL` | URL of created GitHub release | `https://github.com/...` |

The `Is*` flags (`IsSnapshot`, `IsNightly`, `IsHarness`, `IsDraft`,
`IsRelease`, `IsSingleTarget`, `IsMerging`, `IsGitDirty`, `IsGitClean`,
`IsPrepare`) are real booleans, and `NightlyBuild` is a real number — use
them directly:

```yaml
if: "{{ not IsSnapshot }}"            # skip on snapshots
if: "{{ IsHarness }}"                 # only inside the determinism harness
if: "{% if NightlyBuild > 0 %}true{% endif %}"
```

Comparing them to quoted strings (`IsSnapshot == "false"`) never matches —
Tera does not coerce booleans to strings — so anodizer rejects such
conditions with a hard error instead of silently skipping the stage.

### Time

| Variable | Description | Example |
|----------|-------------|---------|
| `Date` | Current date | `2024-01-15` |
| `Timestamp` | Current Unix timestamp | `1705312200` |
| `Now` | Current UTC time (ISO 8601) | `2024-01-15T10:30:00Z` |

### Host runtime

| Variable | Description | Example |
|----------|-------------|---------|
| `RuntimeGoos` | Host OS in Go naming (GoReleaser's `{{ .Runtime.Goos }}` also works) | `linux` |
| `RuntimeGoarch` | Host architecture in anodizer's arch vocabulary — Go names except the mips family, which keeps the Rust spellings (`mipsel`/`mips64el`, not Go's `mipsle`/`mips64le`). GoReleaser's `{{ .Runtime.Goarch }}` also works | `amd64` |
| `RustcVersion` | Host rustc release version; empty when rustc is unavailable | `1.96.0` |

```yaml
# Skip a config on non-amd64 build hosts:
if: '{{ RuntimeGoarch == "amd64" }}'
```

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

## Functions and filters

Tera provides many [built-in filters](https://keats.github.io/tera/docs/#built-in-filters) (`lower`, `upper`, `title`, `trim`, `length`, `default`, …). On top of those, anodizer registers a full set of release-oriented helpers. Most are available in **both forms** — as a filter (`{{ X | fn(...) }}`) and as a function (`{{ fn(s=X, ...) }}`) — so the GoReleaser positional form (`{{ fn X ... }}`) auto-translates onto them.

Examples below use the Tera-native no-dot idiom.

### String

| Helper | Form | Example | Result |
|--------|------|---------|--------|
| `lower` / `tolower` | filter | `{{ Os \| lower }}` | `linux` |
| `upper` / `toupper` | filter | `{{ Os \| upper }}` | `LINUX` |
| `title` | filter / fn | `{{ "hello world" \| title }}` | `Hello World` |
| `trim` | filter / fn | `{{ " x " \| trim }}` | `x` |
| `trimprefix` | filter | `{{ Tag \| trimprefix(prefix="v") }}` | `1.2.3` |
| `trimsuffix` | filter | `{{ File \| trimsuffix(suffix=".tar.gz") }}` | strips suffix |
| `replace` | filter / fn | `{{ Version \| replace(from=".", to="_") }}` | `1_2_3` |
| `split` | filter / fn | `{{ "a.b.c" \| split(sep=".") }}` | `["a","b","c"]` |
| `contains` | filter / fn | `{{ Tag \| contains(substr="rc") }}` | `true` / `false` |
| `slice` | filter / fn | `{{ Tag \| slice(start=1, end=4) }}` | `1.2` (end-exclusive, Go semantics) |
| `reReplaceAll` | fn | `{{ reReplaceAll(pattern="[^0-9]", input=Tag, replacement="") }}` | digits only |
| `urlPathEscape` | fn | `{{ urlPathEscape(s=Branch) }}` | percent-encoded path segment |
| `mdv2escape` | filter | `{{ Body \| mdv2escape }}` | Telegram MarkdownV2-escaped |
| `ruby_escape` | filter | `{{ Desc \| ruby_escape }}` | safe in a Ruby `"…"` literal |

### Formatting

| Helper | Form | Example | Result |
|--------|------|---------|--------|
| `printf` | fn | `{{ printf(format="%s-%s", args=[Os, Arch]) }}` | `linux-amd64` |
| `printf` | fn | `{{ printf(format="%04d", args=[Patch]) }}` | `0003` |
| `print` | fn | `{{ print(args=[Os, Arch]) }}` | `linuxamd64` (Go `Sprint`) |
| `println` | fn | `{{ println(args=[Os, Arch]) }}` | `linux amd64\n` (Go `Sprintln`) |

`printf` implements the Go verb subset `%s %d %v %x %X %o %b %c %q %f %e %E %g %G %t %%` with flags, width, and precision (Go-style exponents). `print` follows Go's `Sprint` spacing rule (a space is inserted between two adjacent operands only when neither is a string).

### Path

| Helper | Form | Example | Result |
|--------|------|---------|--------|
| `dir` | filter | `{{ ArtifactPath \| dir }}` | parent directory |
| `base` | filter | `{{ ArtifactPath \| base }}` | final path component |
| `abs` | filter | `{{ "./dist" \| abs }}` | absolute path |

### List and map

| Helper | Form | Example | Result |
|--------|------|---------|--------|
| `list` | fn | `{{ list(items=[Os, Arch]) \| join(sep="-") }}` | `linux-amd64` |
| `map` | fn | `{% set M = map(pairs=["a", 1]) %}{{ M.a }}` | `1` |
| `index` | fn | `{{ index(collection=Parts, key=0) }}` | element at index |
| `indexOrDefault` | fn | `{{ indexOrDefault(map=M, key="k", default="-") }}` | value or default |
| `in` / `contains_any` | filter / fn | `{{ in(items=["rc", "beta"], value=Prerelease) }}` | `true` / `false` |
| `filter` | fn | `{{ filter(items=Lines, regexp="^v") }}` | matching lines |
| `reverseFilter` | fn | `{{ reverseFilter(items=Lines, regexp="^#") }}` | non-matching lines |
| `englishJoin` | fn | `{{ englishJoin(items=Names) }}` | `a, b, and c` |

### Semver

| Helper | Form | Example | Result |
|--------|------|---------|--------|
| `incpatch` | filter | `{{ Version \| incpatch }}` | `1.2.4` |
| `incminor` | filter | `{{ Version \| incminor }}` | `1.3.0` |
| `incmajor` | filter | `{{ Version \| incmajor }}` | `2.0.0` |

<!-- The Environment/File/Time examples whose arguments are all string literals
     are wrapped in zola's comment-escape (open brace pair + /* ... */ + close):
     zola parses such inline code as a shortcode invocation and fails the build
     on the unknown name. Calls that reference variables (e.g. s=Branch) do not
     parse as shortcodes, are left alone by zola, and need no escape. -->

### Environment

| Helper | Form | Example | Result |
|--------|------|---------|--------|
| `Env.NAME` | var | `{{ Env.GITHUB_TOKEN }}` | env var value |
| `envOrDefault` | fn | `{{/* envOrDefault(name="CI", default="local") */}}` | value or default |
| `isEnvSet` | fn | `{{/* isEnvSet(name="CI") */}}` | `true` / `false` |

### File

| Helper | Form | Example | Result |
|--------|------|---------|--------|
| `readFile` | fn | `{{/* readFile(path="VERSION") */}}` | file contents (empty on error) |
| `mustReadFile` | fn | `{{/* mustReadFile(path="VERSION") */}}` | file contents (errors if missing) |

### Time

| Helper | Form | Example | Result |
|--------|------|---------|--------|
| `time` | fn | `{{/* time(format="2006-01-02") */}}` | current date (Go layout accepted) |
| `now_format` | filter | `{{ Now \| now_format(format="%Y-%m-%d") }}` | current date (chrono format) |
| `date` | filter | `{{ Now \| date(format="%Y%m%d") }}` | `20260703` |

`date` formats a Unix timestamp (integer), an RFC 3339 datetime string, a naive `%Y-%m-%dT%H:%M:%S` datetime, or a plain `%Y-%m-%d` date. `format` takes chrono strftime specifiers (default `%Y-%m-%d`). `timezone` takes an IANA name (`timezone="America/New_York"`) and converts timestamps and offset-carrying RFC 3339 inputs; naive datetime and plain-date inputs format as UTC and ignore it. `locale` is not supported and errors — output is always POSIX-locale.

### Hashing

Fourteen hash functions take a file path argument (`s=`) and return the lowercase hex digest of that file's contents:

`md5` · `crc32` · `sha1` · `sha224` · `sha256` · `sha384` · `sha512` · `sha3_224` · `sha3_256` · `sha3_384` · `sha3_512` · `blake2b` · `blake2s` · `blake3`

```yaml
body_template: "checksum: {{ sha256(s=ArtifactPath) }}"
```

> **Not supported (intentionally):** Go's `html`, `js`, `urlquery`, and `call` builtins are web-escaping / reflection helpers with no role in release templating, so they are not registered. Everything else from GoReleaser's function set — plus the Go `text/template` builtins that matter — is present.

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
