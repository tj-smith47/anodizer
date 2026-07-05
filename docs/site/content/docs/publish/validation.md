+++
title = "Artifact validation"
description = "Offline schema validation of every publisher's rendered artifact before a release"
weight = 90
template = "docs.html"
+++

Before a release uploads anything, anodizer renders each configured publisher's
artifact — the winget manifest set, the scoop JSON, the chocolatey `.nuspec`,
the snap.yaml metadata, the Homebrew formula and cask, the nfpm config plus the
built `.deb`/`.rpm`, the AUR `PKGBUILD` and `.SRCINFO`, the nix derivation and
flake, and more — and validates each one against the destination registry's own
schema and rules. It runs offline and hermetically, so a structural defect (a
wrong-typed value, an out-of-enum field, a missing required key, a malformed
manifest) is caught in `--snapshot` / `--dry-run` instead of after a release
has already pushed a manifest the registry rejects.

This closes the gap between "the inputs are populated" and "the whole rendered
document conforms". A required-field check proves a value is present; artifact
validation proves the *assembled artifact* is one the registry will accept.

## How it runs

Artifact validation runs automatically — no config needed — as part of the
snapshot/dry-run emission-validate pass:

```bash
$ anodizer release --snapshot
```

It is also exercised by `task prepush`, which drives the same snapshot pass, so
a malformed publisher artifact fails the local pre-push gate before it can reach
a real release.

### On a sharded determinism build

When determinism runs as a target-restricted shard matrix (a shard, or a
host-only `--single-target` build), emission-validate checks the emissions the
current shard can satisfy and **self-skips** a publisher whose input archives
this shard did not produce — a cross-platform aggregator (`homebrew`, `nix`) is
validated on the shard that built its inputs, not failed on one that couldn't.
On a **full** build (neither restriction set), the self-skip does not apply for
the index/manifest publishers (`homebrew`, `nix`, `aur`, `krew`, `winget`,
`scoop`, `chocolatey`): a configured one with no eligible artifact still errors,
since it would otherwise generate an installable reference that 404s. The
build-time packagers `nfpm`/`snapcraft` and the image-reference publisher `mcp`
aren't covered by this gate — with no eligible input they simply produce
nothing (or a config-driven skip, for `mcp`), not an error. See
[Emission-validate on sharded builds](../advanced/determinism.md#emission-validate-on-sharded-builds).

## Two layers

Validation runs in two layers per publisher.

**An always-on hermetic floor.** Every artifact is checked with no external
tools and no network:

- Vendored JSON schemas (pinned and embedded at build time) for winget, scoop,
  krew, mcp, snapcraft, and nfpm.
- Pure-Rust structural checks for the artifacts that have no JSON schema: the
  chocolatey nuspec XML, the Homebrew Ruby formula/cask, the AUR
  `PKGBUILD`/`.SRCINFO`, and the nix derivation.

This floor always runs and always reports — it never depends on host tooling.

**Optional stronger checks, gated on tool presence.** When the matching tool is
installed (typical on CI and consumer hosts), anodizer runs a deeper check on
top of the floor:

| Tool | Deeper check |
|------|--------------|
| `xmllint` | Validate the chocolatey `.nuspec` against its XSD |
| `ruby -c` | Syntax-check the Homebrew formula/cask Ruby |
| `bash -n` | Syntax-check the AUR `PKGBUILD` |
| `dpkg-deb -f` / `rpm -qip` | Read back a built `.deb`/`.rpm`'s control fields |
| `nix-instantiate --parse` | Parse the rendered nix expression |

When a gated tool is absent the check is **skipped, never failed** — the
hermetic floor still stands, so a missing tool can never turn into a false
rejection.

## What is checked

| Publisher | Artifact | What is checked |
|-----------|----------|-----------------|
| winget | `version` / `installer` / `defaultLocale` manifests | Microsoft's published JSON schemas (ManifestVersion 1.12.0) |
| scoop | App manifest JSON | The Scoop project's draft-07 manifest schema |
| krew | Plugin manifest | krew's plugin-validation rules (transcribed schema) |
| mcp | `server.json` | The MCP registry's `server.json` schema |
| chocolatey | `.nuspec` XML | Pure-Rust structural floor; XSD via `xmllint` when present |
| snapcraft | `snap.yaml` metadata | snapd's `snap.yaml` validation rules (transcribed schema) |
| homebrew | Formula + cask Ruby | Structural stanza floor; `ruby -c` when present |
| nfpm | nfpm config + built `.deb`/`.rpm` | nfpm's config schema; control fields via `dpkg-deb`/`rpm` when present |
| aur | `PKGBUILD` + `.SRCINFO` | Structural floor; `bash -n` when present |
| nix | Derivation + flake | Structural floor; `nix-instantiate --parse` when present |

## Example: a defect caught in snapshot

A misconfigured field is caught and named before any upload. Each finding is
reported as `publisher: field '<path>' — <what the schema expected>`:

```bash
$ anodizer release --snapshot
...
Error: publisher artifact schema validation failed:
winget: field '/PublisherUrl' — "acme.example" does not match "^([Hh][Tt][Tt][Pp][Ss]?)://.+$"
```

The message names the offending publisher, the JSON-Pointer path to the field,
and the registry schema's own expectation — so the fix is a one-line config edit
rather than a post-release investigation into why a manifest was rejected.

## Schema provenance

The vendored schemas live in `crates/stage-publish/schemas/` and are pinned to
the registry / tool versions anodizer targets. Each is self-contained (no
external `$ref` that would trigger a network fetch), so validation stays
hermetic. Refreshing a schema is a deliberate, reviewed bump documented in
`crates/stage-publish/schemas/SOURCES.md`, which records each schema's source,
pinned version, and refresh procedure.
