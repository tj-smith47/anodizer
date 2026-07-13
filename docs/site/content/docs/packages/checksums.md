+++
title = "Checksums"
description = "Generate cryptographic checksums for all artifacts"
weight = 2
template = "docs.html"
+++

The checksum stage computes cryptographic hashes for all artifacts and writes them to a checksum file.

## Classification

Packager — generates a checksum file alongside release artifacts. Required: not a publisher; always runs unless disabled.

## Minimal config

Checksums are enabled by default with SHA-256. No config needed for basic usage.

## Full config reference

```yaml
defaults:
  checksum:
    name_template: "{{ ProjectName }}_{{ Version }}_checksums.txt"  # optional
    algorithm: sha256                   # optional; sha256 | sha512 | sha1 | blake2b | etc.
    skip: false                         # optional; skip checksum generation
    extra_files: []                     # optional; additional files to checksum
    ids: []                             # optional; only checksum artifacts matching these IDs
    split: false                        # optional; one sidecar per artifact instead of a combined file
    split_format: bare                  # optional; bare | coreutils (only when split: true)
```

When `split: true`, anodizer writes one sidecar per artifact (e.g.
`app-1.0.0-linux-amd64.tar.gz.sha256`) instead of a combined
`checksums.txt`. The sidecar content is controlled by `split_format`:

- `bare` (default) — the raw hex hash only, no filename, no trailing newline
  (matches GoReleaser's split-checksum output).
- `coreutils` — `<hash>  <filename>` with a trailing newline, so each sidecar
  verifies directly with `shasum -c` / `sha256sum -c` from the artifact
  directory. Choose this when migrating from a hand-rolled
  `shasum -a 256 file > file.sha256` step whose consumers run `shasum -c`.

The combined (`split: false`) file is always coreutils-format.

## Authentication

Not applicable — checksum generation is a local build step with no external service calls.

## Common gotchas

- The checksum file aggregates hashes for all artifacts produced up to this stage. Signing runs *after* checksums in the fixed pipeline order (`checksum → attest → sign → release`), so signature files are never listed inside the checksum file — they are themselves separate release assets. This ordering is not configurable.
- `extra_files` adds files to the checksum file without uploading them; ensure they exist at the path specified.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Checksum config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name_template` | string | `{{ ProjectName }}_{{ Version }}_checksums.txt` | Checksum filename |
| `algorithm` | string | `sha256` | Hash algorithm |
| `skip` | bool/template | `false` | Skip checksum generation. The legacy `disable` spelling is accepted as a deprecation-warned alias. |
| `extra_files` | list | none | Additional files to checksum |
| `ids` | list | none | Only checksum artifacts matching these IDs |
| `split` | bool | `false` | Write one sidecar per artifact instead of a combined `checksums.txt` |
| `split_format` | string | `bare` | Sidecar content when `split: true`: `bare` (hash only) or `coreutils` (`<hash>  <filename>`, `shasum -c`-verifiable) |

## Supported algorithms

| Algorithm | Config value |
|-----------|-------------|
| SHA-1 | `sha1` |
| SHA-224 | `sha224` |
| SHA-256 | `sha256` |
| SHA-384 | `sha384` |
| SHA-512 | `sha512` |
| BLAKE2b | `blake2b` |
| BLAKE2s | `blake2s` |
| SHA3-224 | `sha3-224` |
| SHA3-256 | `sha3-256` |
| SHA3-384 | `sha3-384` |
| SHA3-512 | `sha3-512` |
| BLAKE3 | `blake3` |
| CRC-32 | `crc32` |
| MD5 | `md5` |

## Custom config

```yaml
defaults:
  checksum:
    name_template: "{{ ProjectName }}_{{ Version }}_SHA512SUMS"
    algorithm: sha512
```

## Disabling checksums

```yaml
defaults:
  checksum:
    skip: true
```
