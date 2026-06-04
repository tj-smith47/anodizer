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
    - enabled: true
      sign:
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
| `ids` | list | project name | Build IDs to filter |
| `enabled` | string/bool | `false` | Enable this config (must be explicitly enabled) |

#### Sign config (`sign`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `certificate` | string | none | Path to .p12 certificate or base64 contents (template) |
| `password` | string | none | Password for the .p12 certificate (template) |
| `entitlements` | string | none | Path to entitlements XML file (template) |

#### Notarize config (`notarize`, optional — omit for sign-only)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `issuer_id` | string | none | App Store Connect API key issuer UUID (template) |
| `key` | string | none | Path to .p8 key file or base64 contents (template) |
| `key_id` | string | none | API key ID (template) |
| `timeout` | string | `10m` | Timeout for notarization polling |
| `wait` | bool | none | Whether to wait for notarization to complete |

## Native mode (codesign + xcrun)

macOS only. Uses Keychain identities for signing and `xcrun notarytool` for notarization.

### Minimal config

```yaml
notarize:
  macos_native:
    - enabled: true
      use: dmg
      sign:
        identity: "Developer ID Application: My Org"
      notarize:
        profile_name: my-notarytool-profile
```

### Native config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `ids` | list | project name | Build IDs to filter |
| `enabled` | string/bool | `false` | Enable this config (must be explicitly enabled) |
| `use` | string | `dmg` | Artifact type to sign: `"dmg"` or `"pkg"` |

#### Sign config (`sign`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `identity` | string | none | Keychain identity (template) |
| `keychain` | string | none | Path to Keychain file (template) |
| `options` | list | none | Options for codesign (e.g., `["runtime"]`). DMG only |
| `entitlements` | string | none | Path to entitlements XML (template). DMG only |

#### Notarize config (`notarize`, required)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `profile_name` | string | none | `notarytool` stored credentials profile name (template) |
| `wait` | bool | none | Whether to wait for notarization to complete |
| `timeout` | string | `10m` | Timeout for `xcrun notarytool submit --timeout` (template) |

## Behavior

- Notarization must be explicitly enabled (`enabled: true`) — it is off by default
- After signing, SHA-256 checksums are refreshed for all affected darwin binary artifacts
- Notarization status is differentiated: accepted, invalid, rejected, and timeout
- Timeouts are treated as non-fatal (the submission may still be processing)
- Sensitive values (P12 password, API key file path) are redacted from log output
- In native DMG mode, when `wait` is enabled, the notarization ticket is automatically stapled to the DMG
- Skippable with `--skip notarize`

## Both modes together

You can use cross-platform signing for CI builds and native signing for local builds:

```yaml
notarize:
  macos:
    - enabled: "{{ ne .Env.CI \"\" }}"
      sign:
        certificate: "{{ Env.P12_CERTIFICATE }}"
        password: "{{ Env.P12_PASSWORD }}"
  macos_native:
    - enabled: "{{ eq .Env.CI \"\" }}"
      sign:
        identity: "Developer ID Application: My Org"
```
