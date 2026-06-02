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

Per-publisher validators add their row above as each is implemented.
