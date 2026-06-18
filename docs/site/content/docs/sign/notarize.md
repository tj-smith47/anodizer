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
    - skip: false
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
| `skip` | string/bool | `true` | Skip this config. **Canonical** field — notarization is off until you set `skip: false`. The upstream-style `enabled:` is accepted as an inverting back-compat alias (`enabled: true` ⇒ `skip: false`). |

#### Sign config (`sign`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `certificate` | string | none | Path to .p12 certificate or base64 contents (template) |
| `password` | string | none | Password for the .p12 certificate (template) |
| `entitlements` | string | none | Path to entitlements XML file (template) |
| `timestamp_url` | string | `http://timestamp.apple.com/ts01` | RFC-3161 timestamp service URL passed to `rcodesign sign --timestamp-url`. Override when running behind a corporate proxy or when Apple's service is unreachable. |

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
    - skip: false
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
| `skip` | string/bool | `true` | Skip this config. **Canonical** field — set `skip: false` to enable. The upstream-style `enabled:` is accepted as an inverting back-compat alias. |
| `use` | string | `dmg` | Artifact type to sign: `"dmg"` or `"pkg"` |

#### Sign config (`sign`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `identity` | string | none | Keychain identity (template) |
| `keychain` | string | none | Path to Keychain file (template) |
| `options` | list | none | Options for codesign (e.g., `["runtime"]`). DMG only |
| `entitlements` | string | none | Path to entitlements XML (template). DMG only |

#### Notarize config (`notarize`, optional — omit for sign-only)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `profile_name` | string | none | `notarytool` stored credentials profile name (template) |
| `wait` | bool | none | Whether to wait for notarization to complete |
| `timeout` | string | `10m` | Timeout for `xcrun notarytool submit --timeout` (template) |

## Behavior

- Notarization must be explicitly enabled (`skip: false`, or the `enabled: true` back-compat alias) — it is off by default
- After signing, SHA-256 checksums are refreshed for all affected darwin binary artifacts
- Notarization status is differentiated: accepted, invalid, rejected, and timeout
- Timeouts are treated as non-fatal (the submission may still be processing)
- Sensitive values (P12 password, API key file path) are redacted from log output
- In native mode, when `wait` is enabled, the notarization ticket is automatically stapled to the artifact — both DMG (`use: dmg`) and PKG (`use: pkg`) are stapled
- Skippable with `--skip notarize`

## Both modes together

You can use cross-platform signing for CI builds and native signing for local builds:

```yaml
notarize:
  macos:
    # cross-platform path runs in CI (skip when NOT in CI)
    - skip: "{{ Env.CI == \"\" }}"
      sign:
        certificate: "{{ Env.P12_CERTIFICATE }}"
        password: "{{ Env.P12_PASSWORD }}"
  macos_native:
    # native path runs locally (skip when IN CI)
    - skip: "{{ Env.CI != \"\" }}"
      sign:
        identity: "Developer ID Application: My Org"
```
