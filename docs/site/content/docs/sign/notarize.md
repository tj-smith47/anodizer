+++
title = "macOS Notarization"
description = "Sign and notarize macOS binaries for Gatekeeper compliance"
weight = 3
template = "docs.html"
+++

Anodizer supports macOS code signing and notarization in two modes: cross-platform (via `rcodesign`, works on any OS) and native (via `codesign` + `xcrun notarytool`, macOS only).

## Cross-platform mode (rcodesign)

Works on Linux, macOS, and Windows. Uses a P12 certificate for signing and an App Store Connect API key for notarization.

### Minimal config

```yaml
notarize:
  macos:
    - sign:
        certificate: "{{ Env.P12_CERTIFICATE }}"
        password: "{{ Env.P12_PASSWORD }}"
      notarize:
        issuer_id: "{{ Env.NOTARIZE_ISSUER_ID }}"
        key: "{{ Env.NOTARIZE_KEY }}"
        key_id: "{{ Env.NOTARIZE_KEY_ID }}"
```

### Cross-platform config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `ids` | list | project name | Build IDs whose darwin binaries and universal binaries this entry signs. With no project name, every darwin binary. |
| `skip` | string/bool | `false` | Skip this config. An entry with no `skip` runs. A template skips when it renders `true` or `1`, trimmed and in any letter case. `enabled:` is accepted as an inverted alias (`enabled: true` is `skip: false`); see [The `enabled` alias](#the-enabled-alias) for how its value is read. |

#### Sign config (`sign`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `certificate` | string | none | Path to .p12 certificate or base64 contents (template) |
| `password` | string | none | Password for the .p12 certificate (template) |
| `entitlements` | string | none | Path to entitlements XML file (template) |
| `timestamp_url` | string | `http://timestamp.apple.com/ts01` | RFC-3161 timestamp service URL passed to `rcodesign sign --timestamp-url`. Override when running behind a corporate proxy or when Apple's service is unreachable. |

#### Notarize config (`notarize`, optional)

Omit this block to sign without notarizing.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `issuer_id` | string | none | App Store Connect API key issuer UUID (template) |
| `key` | string | none | Path to .p8 key file or base64 contents (template) |
| `key_id` | string | none | API key ID (template) |
| `timeout` | string | `10m` | Timeout for notarization polling, passed to `rcodesign notary-submit --max-wait` when `wait` is `true`. Maximum `20m` — the App Store Connect token lifetime is derived from it and Apple rejects tokens that live longer, so a larger value fails config validation. |
| `wait` | bool | `false` | Whether to wait for notarization to complete |

## Native mode (codesign + xcrun)

macOS only. Uses Keychain identities for signing and `xcrun notarytool` for notarization.

### Minimal config

```yaml
notarize:
  macos_native:
    - use: dmg
      sign:
        identity: "Developer ID Application: My Org"
      notarize:
        profile_name: my-notarytool-profile
```

### Native config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `ids` | list | project name | Build IDs whose darwin artifacts this entry signs and notarizes: app bundles and DMGs with `use: dmg`, PKGs with `use: pkg`. With no project name, every matching artifact. |
| `skip` | string/bool | `false` | Skip this config. An entry with no `skip` runs. A template skips when it renders `true` or `1`, trimmed and in any letter case. `enabled:` is accepted as an inverted alias; see [The `enabled` alias](#the-enabled-alias). |
| `use` | string | `dmg` | Artifact type to sign: `"dmg"` or `"pkg"` |

#### Sign config (`sign`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `identity` | string | none | Keychain identity (template) |
| `keychain` | string | none | Path to Keychain file (template) |
| `options` | list | none | Options for codesign (e.g., `["runtime"]`). DMG only |
| `entitlements` | string | none | Path to entitlements XML (template). DMG only |

#### Notarize config (`notarize`, required)

Native mode always notarizes: an entry with no `notarize` block, or with no
`profile_name`, fails the stage.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `profile_name` | string | none | `notarytool` stored credentials profile name (template). Required. |
| `wait` | bool | `false` | Whether to wait for notarization to complete. Stapling runs only when this is `true`. |
| `timeout` | string | `10m` | Duration passed to `xcrun notarytool submit --timeout` (for example `30m`). Read as written; it is not a template. |

## Behavior

- Every `macos` or `macos_native` entry runs unless its `skip` is truthy (or its `enabled` is falsy). A top-level `notarize.skip` turns off the whole stage.
- `macos` and `macos_native` cannot both hold entries: they sign the same binaries with different tools, so the stage fails before signing anything when both are set.
- Each entry needs a `sign` block. Cross-platform mode requires `sign.certificate`; native mode requires `sign.identity` and a `notarize` block with `profile_name`.
- When `ids` matches nothing, the stage prints a warning and moves on; under `--strict` it fails. Cross-platform mode matches darwin binaries and universal binaries. Native mode matches app bundles and DMGs with `use: dmg`, and PKGs with `use: pkg`.
- With `use: dmg`, app bundles that were signed while no DMG matched print a warning, and fail under `--strict`, because nothing is sent for notarization.
- After signing, SHA-256 checksums are refreshed for every signed darwin binary, universal binary, DMG and PKG
- Notarization status is differentiated: accepted, invalid, rejected, and timeout
- Timeouts are treated as non-fatal (the submission may still be processing)
- Sensitive values (P12 password, API key file path) are redacted from log output
- In cross-platform mode, a `sign.certificate` or `notarize.key` given as base64 contents is decoded into a temporary file whose path is passed to `rcodesign`. The file is deleted when the config's notarization finishes, whether it succeeded or failed. A value that is a file path is passed through as it is and never deleted.
- In native mode, when `wait` is enabled, the notarization ticket is automatically stapled to the artifact — both DMG (`use: dmg`) and PKG (`use: pkg`) are stapled
- Skippable with `--skip notarize`

## The `enabled` alias

`enabled:` is the inverse of `skip:` and takes a boolean or a template. A
template is rendered and read in any letter case: `false`, `0`, `no` or an
empty render turns the config off, and `true`, `1`, `yes` or any other render
turns it on. A literal `enabled: ""` is not a template, sets no gate, and
notarizes. The [bool-or-template table](@/docs/general/templates.md#bool-or-template-values)
lists this reading beside the other fields.

```yaml
notarize:
  macos:
    - enabled: "{{ if IsSnapshot }}false{{ else }}true{{ end }}"  # notarize real releases only
      sign:
        certificate: "{{ Env.P12_CERTIFICATE }}"
        password: "{{ Env.P12_PASSWORD }}"
```
