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
| krew | `krew.v1alpha2.schema.json` | **Derived** from <https://github.com/kubernetes-sigs/krew/blob/299f8e0/internal/index/validation/validate.go> + <https://github.com/kubernetes-sigs/krew/blob/299f8e0/pkg/index/types.go> | Transcribed from krew `299f8e0` (no upstream JSON Schema exists — krew validates manifests in Go) | Re-read `validate.go` (`ValidatePlugin` / `validatePlatform` / `validateSelector`) and `types.go` at krew HEAD and reconcile any rule changes into this file. This is an **authored** transcription, not a vendored upstream document: it is self-contained (draft 2020-12, internal `$defs` only) and is never re-downloaded. |
| mcp | `mcp.server.2025-12-11.schema.json` | <https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json> | server.json schema `2025-12-11` (draft-07) | Re-download from the source URL into this file. Bump **only alongside** the `CURRENT_SCHEMA_URL` constant in `src/mcp/manifest.rs` and rename the file to match the new date — the `schema_version_lockstep` test asserts the constant's date matches the vendored filename and the schema's `$id`. The schema is fully internal-`$ref` (`#/definitions/*`, no network fetch) — re-verify that property after any bump. |

Per-publisher validators add their row above as each is implemented.

The krew row's "Source URL" points at upstream validation **code**, not a schema
file: krew enforces plugin-manifest shape via `ValidatePlugin` in Go and ships no
standalone JSON Schema. The vendored file is a faithful transcription of those Go
rules, so refreshing means re-reading the validators rather than re-downloading.
