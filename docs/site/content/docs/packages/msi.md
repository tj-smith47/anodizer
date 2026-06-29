+++
title = "MSI"
description = "Build Windows installer packages using WiX Toolset"
weight = 62
template = "docs.html"
+++

The MSI stage builds `.msi` Windows installer packages from your Windows binaries using the [WiX Toolset](https://wixtoolset.org/). Installers are placed in `dist/windows/`.

## Classification

Packager — builds Windows MSI installers from Windows binaries. Required: not a publisher; runs only on Windows targets.

## Minimal config

```yaml
crates:
  - name: myapp
    msis:
      - wxs: installer/myapp.wxs
```

## Full config reference

```yaml
crates:
  - name: myapp
    msis:
      - wxs: installer/myapp.wxs     # required; path to .wxs file (template)
        id: ""                        # optional; unique identifier
        ids: []                       # optional; filter by build IDs
        name: ""                      # optional; output filename template
        version: ""                   # optional; v3 | v4 | wixl (alias: linux); auto-detected if omitted
        replace: false                # optional; remove archive artifacts, keep MSI only
        mod_timestamp: ""             # optional; fixed timestamp for reproducible builds (template)
        amd64_variant: ""             # optional; amd64 variant filter (v1/v2/v3/v4)
        extra_files: []               # optional; plain filename strings copied into the WiX build context
        extensions: []                # optional; WiX extensions to enable (template per entry)
        if: ""                        # optional; skip this config if rendered result is falsy
        disable: false                # optional (alias for skip)
        hooks:
          before: []                  # optional; commands to run before the WiX build
          after: []                   # optional; commands to run after the WiX build
```

## Authentication

Not applicable — MSI generation is a local build step with no external service calls.

## Common gotchas

- **WiX must be on `PATH`**: the stage probes for `wix` (v4), then `candle` (v3), then `wixl` (the Linux-native [msitools](https://wiki.gnome.org/msitools) path). If none is found, the stage errors immediately.
- **`.wxs` template rendering**: the WiX source file is rendered through Tera before being passed to WiX. Ensure any `{{ ... }}` expressions in the `.wxs` are valid Tera.
- **Architecture mapping**: `amd64` → `x64`, `386`/`i686` → `x86`, `arm64`/`aarch64` → `arm64`. WiX build commands vary by arch.
- **Stable `UpgradeCode`**: see the [Stable IDs and the upgrade chain](#stable-ids-and-the-upgrade-chain) section below — this is the most common cause of broken upgrades.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Required tools

| Tool | Version | Notes |
|------|---------|-------|
| `wix` | v4 | Unified `wix build` command. |
| `candle` + `light` | v3 | Two-step compilation. |
| `wixl` | msitools | Linux-native MSI build (from GNOME msitools); consumes a WiX v3-dialect `.wxs`. Does not support WiX `-ext` extensions. |

WiX v4 is preferred. If only v3 tools are found (`candle` and `light` on `PATH`), the v3 workflow is used automatically. On Linux, where neither `wix` nor `candle`/`light` exists, anodizer builds the same `.wxs` through `wixl` — letting you produce MSIs without Windows. Select it explicitly with `version: wixl` (or `version: linux`).

## Platform

MSI only processes binary artifacts targeting Windows. Binaries for other operating systems are ignored.

## WiX version auto-detection

The WiX version is determined in this order:

1. **`version` field** — if set in config (`v3`, `v4`, or `wixl`/`linux`), that toolchain is used.
2. **`.wxs` content** — if the file contains `http://schemas.microsoft.com/wix/2006/wi`, WiX v3 is selected; the `http://wixtoolset.org/schemas/v4/wxs` namespace or no namespace defaults to v4.
3. **Installed tools** — checks for `wix` (v4), then `candle` (v3), then `wixl` (Linux msitools) on `PATH`.

## Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | | Unique identifier for referencing this config from other stages. |
| `ids` | list | all | Filter to specific build IDs. |
| `wxs` | string | **required** | Path to the WiX source file (`.wxs`). The path itself and the file contents are both rendered through the template engine. |
| `name` | string | `{{ ProjectName }}_{{ MsiArch }}` | Output filename template. `.msi` is appended automatically when absent. |
| `version` | string | auto | WiX toolchain: `v3`, `v4`, or `wixl` (alias `linux`) for the Linux-native msitools path. Auto-detected from the `.wxs` namespace and installed tools when omitted. |
| `replace` | bool | `false` | Remove matching archive artifacts, keeping only the MSI. |
| `mod_timestamp` | string | | Fixed timestamp for reproducible builds. Templates allowed (e.g. `{{ CommitTimestamp }}`). |
| `amd64_variant` | string | | amd64 microarchitecture variant filter (`v1`/`v2`/`v3`/`v4`). |
| `extra_files` | list | | Plain filename strings (not `{src, dst}` objects) copied into the WiX build context alongside the rendered `.wxs`. |
| `extensions` | list | | WiX extensions to enable (e.g. `WixUIExtension`). Each entry is a template. |
| `if` | string | | Skip this config when the rendered value is `false` or empty. Anodizer-additive — see note below. |
| `disable` / `skip` | bool or string | `false` | Skip this MSI config. Accepts a bool or a template string. |
| `hooks.before` | list | | Commands to run before the WiX build. |
| `hooks.after` | list | | Commands to run after the WiX build. Receives `ArtifactPath`, `ArtifactName`, and `ArtifactExt`. |

## Template variables in `.wxs`

The `.wxs` file is rendered through the Tera template engine before being passed to WiX. Available variables include:

| Variable | Description |
|----------|-------------|
| `{{ ProjectName }}` | Project/crate name. |
| `{{ Version }}` | Full release version (may include a pre-release / build-metadata suffix). Use for display fields (Description, Comments) — **not** for `Product/@Version`. |
| `{{ MsiVersion }}` | Numeric `major.minor.patch` core of `Version`, each field clamped to WiX's `0..=65534`. Use this for `Product/@Version` (v3) / `Package/@Version` (v4): WiX rejects a non-numeric version (`candle CNDL0108`), so a pre-release like `1.0.0-rc.1` (or a determinism snapshot) must be coerced. |
| `{{ Arch }}` | Architecture: `amd64`, `arm64`, etc. |
| `{{ MsiArch }}` | MSI architecture identifier: `x64`, `x86`, `arm64`. |
| `{{ BinaryPath }}` | Full path to the binary being packaged. |
| `{{ Os }}` | Operating system (`windows`). |
| `{{ Tag }}` | Git tag. |

## Architecture mapping

| Build arch | MSI arch |
|------------|----------|
| `amd64` / `x86_64` | `x64` |
| `386` / `i686` / `i386` | `x86` |
| `arm64` / `aarch64` | `arm64` |

## WiX v4 build command

```
wix build installer/myapp.wxs -o dist/windows/myapp_x64.msi
```

## WiX v3 build commands

```
candle -nologo installer/myapp.wxs -o dist/windows/myapp_x64.wixobj
light  -nologo dist/windows/myapp_x64.wixobj -o dist/windows/myapp_x64.msi
```

## Hooks

Pre- and post-build hooks let you run arbitrary commands around the WiX toolchain.

```yaml
msis:
  - wxs: installer/myapp.wxs
    hooks:
      before:
        - make generate-resources
      after:
        - codesign {{ ArtifactPath }}
```

### Post-hook template variables

Post-hooks (`after:`) receive the following additional template variables that describe the produced artifact:

| Variable | Description |
|----------|-------------|
| `{{ ArtifactPath }}` | Full path to the produced `.msi` file. |
| `{{ ArtifactName }}` | Filename only (e.g. `myapp_x64.msi`). |
| `{{ ArtifactExt }}` | Extension (`.msi`). |

Pre-hooks (`before:`) do not receive these variables — no `.msi` exists yet when they run.

### Hook failure behavior

A failing `before:` hook aborts the entire MSI stage for that crate, matching `before:` semantics in adjacent stages. A failing `after:` hook likewise aborts the stage.

## WiX v3 extensions behavior

When `extensions:` are configured, anodizer passes them to both `candle` **and** `light` in the v3 workflow (upstream GoReleaser passes them only to `candle`). This is an intentional superset: passing extensions to `light` is harmless when they supply only candle-side transforms, and avoids link-time `ExtensionRequired` errors for extensions that also supply linker transforms.

## `if:` condition (anodizer-additive)

The `if:` field is present on this stage as an anodizer extension. GoReleaser Pro's MSI documentation does not include `if:` (it appears on `pkg`, `dmg`, and `appbundle` instead). The field is supported here for consistency with other stages; it is safe to use and will not conflict with GoReleaser config imports.

## Stable IDs and the upgrade chain

Windows Installer uses GUIDs to identify products and track upgrade history. Getting these wrong causes failed or duplicated installs.

**Rules:**

- `UpgradeCode` **must** be a hardcoded GUID that never changes across releases. It is the stable identity of your product.
- `Package.Id` (WiX v4) and `Product.Id` (WiX v3) **must not** use `Id="*"` (auto-generate) if you want upgrades to work correctly. A fresh GUID per build means Windows cannot detect that the new package replaces the old one — it installs side-by-side instead of upgrading.

**Recommended pattern:**

```xml
<!-- WiX v4 -->
<Package Name="{{ ProjectName }}"
         Version="{{ MsiVersion }}"
         Manufacturer="My Company"
         UpgradeCode="XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX">
  <MajorUpgrade DowngradeErrorMessage="A newer version is already installed." />
  ...
</Package>
```

```xml
<!-- WiX v3 -->
<Product Id="YYYYYYYY-YYYY-YYYY-YYYY-YYYYYYYYYYYY"
         Name="{{ ProjectName }}"
         Version="{{ MsiVersion }}"
         Manufacturer="My Company"
         UpgradeCode="XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX">
  <MajorUpgrade />
  ...
</Product>
```

Generate your GUIDs once (e.g. with `uuidgen` or an online generator) and commit them as constants in your `.wxs` file.

## Full example

```yaml
crates:
  - name: myapp
    msis:
      - wxs: installer/myapp.wxs
        name: "{{ ProjectName }}_{{ MsiArch }}"
        version: v4
        replace: true
        mod_timestamp: "{{ CommitTimestamp }}"
        extensions:
          - WixUIExtension
        hooks:
          before:
            - make generate-wix-resources
          after:
            - codesign {{ ArtifactPath }}
```

### Minimal `.wxs` template (WiX v4)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">
  <Package Name="{{ ProjectName }}"
           Version="{{ Version }}"
           Manufacturer="My Company"
           UpgradeCode="YOUR-UPGRADE-GUID-HERE">
    <MajorUpgrade DowngradeErrorMessage="A newer version is already installed." />
    <MediaTemplate EmbedCab="yes" />
    <Feature Id="ProductFeature">
      <ComponentGroupRef Id="ProductComponents" />
    </Feature>
    <ComponentGroup Id="ProductComponents" Directory="INSTALLFOLDER">
      <Component>
        <File Source="{{ BinaryPath }}" />
      </Component>
    </ComponentGroup>
  </Package>
</Wix>
```
