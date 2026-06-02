# Vendored publisher registry schemas

Each package-manager publisher's destination registry publishes a schema that
constrains the manifest shape it will accept. This directory vendors those
schemas — pinned to a specific upstream version/commit and embedded at build
time via `include_str!` — so artifact validation runs fully **offline**, with
no network fetch and no drift against a moving upstream.

Convention: one file per publisher named `<publisher>.<json|xsd>`, where the
extension matches the schema language (`.json` for JSON Schema, `.xsd` for the
XML Schema chocolatey nuspec uses). Refreshing a schema is a deliberate,
reviewed bump: update the file, update its row below, and re-run the affected
publisher's schema test.

Each vendored schema must be **self-contained** — no external `$ref` that would
trigger a network fetch at compile time. Inline any referenced subschema when
vendoring so validation stays hermetic.

| Publisher | File | Source URL | Version/commit | How to refresh |
|-----------|------|------------|----------------|----------------|
| winget | `winget.version.1.12.0.schema.json` | <https://raw.githubusercontent.com/microsoft/winget-cli/master/schemas/JSON/manifests/v1.12.0/manifest.version.1.12.0.json> | ManifestVersion 1.12.0 (winget-cli `efcb928`) | Re-download from the source URL into this file. Bump only alongside the `ManifestVersion` the `crate::winget` renderer emits. |
| winget | `winget.installer.1.12.0.schema.json` | <https://raw.githubusercontent.com/microsoft/winget-cli/master/schemas/JSON/manifests/v1.12.0/manifest.installer.1.12.0.json> | ManifestVersion 1.12.0 (winget-cli `efcb928`) | Re-download from the source URL into this file. Bump only alongside the `ManifestVersion` the `crate::winget` renderer emits. |
| winget | `winget.defaultLocale.1.12.0.schema.json` | <https://raw.githubusercontent.com/microsoft/winget-cli/master/schemas/JSON/manifests/v1.12.0/manifest.defaultLocale.1.12.0.json> | ManifestVersion 1.12.0 (winget-cli `efcb928`) | Re-download from the source URL into this file. Bump only alongside the `ManifestVersion` the `crate::winget` renderer emits. |
| scoop | `scoop.schema.json` | <https://raw.githubusercontent.com/ScoopInstaller/Scoop/master/schema.json> | draft-07 app-manifest schema (Scoop `7e3dc73`) | Re-download from the source URL into this file. The schema is fully internal-`$ref` (no network fetch) — re-verify that property after any bump. |

Per-publisher validators add their row above as each is implemented.
