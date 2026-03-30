+++
title = "MSI"
description = "Build Windows installer packages using WiX Toolset"
weight = 62
template = "docs.html"
+++

The MSI stage builds `.msi` Windows installer packages from your Windows binaries using the [WiX Toolset](https://wixtoolset.org/). Installers are placed in `dist/windows/`.

## Required tools

| Tool | Version | Notes |
|------|---------|-------|
| `wix` | v4 | Unified `wix build` command. |
| `candle` + `light` | v3 | Two-step compilation. |

WiX v4 is preferred. If only v3 tools are found (`candle` and `light` on `PATH`), the v3 workflow is used automatically.

## Platform

MSI only processes binary artifacts targeting Windows. Binaries for other operating systems are ignored.

## WiX version auto-detection

The WiX version is determined in this order:

1. **`version` field** — if set in config (`v3` or `v4`), that version is used.
2. **`.wxs` content** — if the file contains `http://schemas.microsoft.com/wix/2006/wi`, WiX v3 is selected; the `http://wixtoolset.org/schemas/v4/wxs` namespace or no namespace defaults to v4.
3. **Installed tools** — checks for `wix` (v4) then `candle` (v3) on `PATH`.

## Minimal config

```yaml
crates:
  - name: myapp
    msis:
      - wxs: installer/myapp.wxs
```

## Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | | Unique identifier for referencing this config from other stages. |
| `ids` | list | all | Filter to specific build IDs. |
| `wxs` | string | **required** | Path to the WiX source file (`.wxs`). Rendered through the template engine. |
| `name` | string | `<ProjectName>_<Version>_<MsiArch>.msi` | Output filename template. |
| `version` | string | auto | WiX schema version: `v3` or `v4`. Auto-detected if omitted. |
| `replace` | bool | `false` | Remove matching archive artifacts, keeping only the MSI. |
| `mod_timestamp` | string | | Fixed timestamp for reproducible builds. |
| `disable` | bool | `false` | Skip this MSI config. |

## Template variables in `.wxs`

The `.wxs` file is rendered through the Tera template engine before being passed to WiX. Available variables include:

| Variable | Description |
|----------|-------------|
| `{{ ProjectName }}` | Project/crate name. |
| `{{ Version }}` | Release version. |
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
wix build installer/myapp.wxs -o dist/windows/myapp_1.0.0_x64.msi
```

## WiX v3 build commands

```
candle -nologo installer/myapp.wxs -o dist/windows/myapp_1.0.0_x64.wixobj
light  -nologo dist/windows/myapp_1.0.0_x64.wixobj -o dist/windows/myapp_1.0.0_x64.msi
```

## Full example

```yaml
crates:
  - name: myapp
    msis:
      - wxs: installer/myapp.wxs
        name: "{{ ProjectName }}_{{ Version }}_{{ MsiArch }}.msi"
        version: v4
        replace: true
        mod_timestamp: "{{ .CommitTimestamp }}"
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
