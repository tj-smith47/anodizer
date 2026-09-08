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
        formats: [tar.gz]               # optional; one archive per listed format
                                        # tar.gz | tar.xz | tar.zst | tar | zip | gz | xz | binary | none
                                        # aliases: tgz | txz | tzst
        format_overrides:               # optional; per-OS format overrides
          - os: windows
            formats: [zip]
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
| `formats` | list | `[tar.gz]` | Archive formats; one archive is produced per entry. Values: `tar.gz`, `tar.xz`, `tar.zst`, `tar`, `zip`, `gz`, `xz`, `binary`, `none` (aliases: `tgz`, `txz`, `tzst`). The singular `format` is a deprecated alias folded into `formats` with a warning. |
| `format_overrides` | list | none | Per-OS format overrides (each entry takes `os` plus `formats`) |
| `files` | list | none | Extra files to include (e.g., `LICENSE`, `README.md`) |
| `binaries` | list | all | Specific binaries to include (default: all from builds) |
| `wrap_in_directory` | string | none | Wrap contents in a subdirectory |

## Format overrides

Use different formats for different operating systems:

```yaml
archives:
  - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
    formats: [tar.gz]
    format_overrides:
      - os: windows
        formats: [zip]
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

Use `formats: [binary]` to skip archiving and distribute the raw binaries:

```yaml
archives:
  - formats: [binary]
```

One output per binary, per target. Each is named by rendering `name_template`
with that binary's `{{ .Binary }}` — the default is
`{{ .Binary }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}` (plus the
micro-architecture suffix where one applies), so a crate shipping `myapp` and
`myhelper` for two targets produces four files:

```console
$ anodizer release --snapshot
dist/myapp_1.0.0_linux_amd64
dist/myapp_1.0.0_darwin_arm64
dist/myhelper_1.0.0_linux_amd64
dist/myhelper_1.0.0_darwin_arm64
```

A template that omits `{{ .Os }}` / `{{ .Arch }}` renders one path for several
targets and is rejected, the same way it is for container formats.

Extra files are ignored under this format — a raw binary has no container to
carry a `LICENSE` in — so `files:`, `templated_files:` and the auto-included
LICENSE/README/CHANGELOG are all dropped. Configuring them explicitly is not an
error (`--strict` included); it earns a `-v` note:

```yaml
archives:
  - formats: [binary]
    files:
      - LICENSE
```

```console
     • binary format ignores the files: entries for crate 'myapp' target 'x86_64-unknown-linux-gnu'
```

Windows targets keep the `.exe` suffix. `before:` / `after:` archive hooks do
not fire for `binary`, since there is no archive to post-process. An `archives:`
entry that selects no binaries at all (`meta: true`) produces nothing under
`binary` and says so:

```console
     • skipped archive for myapp/unknown — meta archive under format: binary carries no binaries
```

### Changed in this release

`binary` outputs are now named per binary from `{{ .Binary }}`, where they were
previously named from the archive `name_template` (`{{ .ProjectName }}`) for a
single binary and from the bare file name for several:

| Entry ships… | Old asset | New asset |
|---|---|---|
| one binary `myapp`, project `myapp` | `dist/myapp_1.0.0_linux_amd64` | `dist/myapp_1.0.0_linux_amd64` |
| one binary `mytool`, project `my-tool` | `dist/my-tool_1.0.0_linux_amd64` | `dist/mytool_1.0.0_linux_amd64` |
| binaries `myapp` + `myhelper` | `dist/myapp`, `dist/myhelper` | `dist/myapp_1.0.0_linux_amd64`, `dist/myhelper_1.0.0_linux_amd64` |

Anything that hard-codes the old asset name — a `cargo binstall` `pkg_url`, an
install script, a download URL in a README — must be updated to the new one.
Setting `name_template:` on the entry still wins, so an entry shipping a single
binary can pin its old name in one line:

```yaml
archives:
  - formats: [binary]
    name_template: "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}"
```

A template without `{{ .Binary }}` renders one path for every binary the entry
selects, so an entry shipping two or more binaries is rejected rather than
letting one overwrite the other.

## Re-running over a populated `dist/`

Archiving is idempotent. A re-run — `release` after `release --prepare`, a
retried `release --merge`, or any run over a `dist/` a previous attempt already
populated — rewrites its own archives rather than refusing:

```console
$ anodizer release --split                    # writes dist/myapp-1.0.0-linux-amd64.tar.gz
$ anodizer release --merge --verbose           # converges over it
     • replacing existing archive 'myapp-1.0.0-linux-amd64.tar.gz' left by an earlier run
     • creating ./dist/myapp-1.0.0-linux-amd64.tar.gz
```

A `name_template` that renders the same filename twice **within one run** is
still a hard error — that is a config defect, not leftover state, and it is
caught in `--dry-run` and `--snapshot` too:

```text
archives: name template '{{ ProjectName }}' rendered the same archive
'myapp.tar.gz' more than once for crate 'myapp', so one build target would
silently overwrite another. Add '{{ .Arch }}' to the `name` …
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
        formats: [tar.gz]
        format_overrides:
          - os: windows
            formats: [zip]
        files: [LICENSE, README.md]
        wrap_in_directory: "{{ ProjectName }}-{{ Version }}"
```
