# `format: binary` asset naming (2026-09)

Archive entries with `formats: [binary]` name each output from
`{{ .Binary }}`. Before this change the name came from the archive
`name_template` (`{{ .ProjectName }}` by default) when the entry shipped a
single binary, and from the bare file name when it shipped several.

## What the asset names become

| Entry ships… | Old asset | New asset |
|---|---|---|
| one binary `myapp`, project `myapp` | `dist/myapp_1.0.0_linux_amd64` | `dist/myapp_1.0.0_linux_amd64` |
| one binary `mytool`, project `my-tool` | `dist/my-tool_1.0.0_linux_amd64` | `dist/mytool_1.0.0_linux_amd64` |
| binaries `myapp` + `myhelper` | `dist/myapp`, `dist/myhelper` | `dist/myapp_1.0.0_linux_amd64`, `dist/myhelper_1.0.0_linux_amd64` |

Anything that hard-codes the old asset name — a `cargo binstall` `pkg_url`, an
install script, a download URL in a README — has to name the new one.

## Keeping the old name

Setting `name_template:` on the entry still wins, so an entry shipping a single
binary can pin its old name in one line:

```yaml
archives:
  - formats: [binary]
    name_template: "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}"
```

A template without `{{ .Binary }}` renders one path for every binary the entry
selects, so an entry shipping two or more binaries is rejected rather than
letting one overwrite the other. See
[Archives](../site/content/docs/packages/archives.md) for the current naming
rules and the collision message.
