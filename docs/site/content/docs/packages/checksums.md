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
    disable: false                      # optional; skip checksum generation
    extra_files: []                     # optional; additional files to checksum
    ids: []                             # optional; only checksum artifacts matching these IDs
```

## Authentication

Not applicable — checksum generation is a local build step with no external service calls.

## Common gotchas

- The checksum file aggregates hashes for all artifacts produced up to this stage. Stages that run after checksums (e.g., signing) produce additional artifacts that are not covered unless the checksum stage is configured to run after signing.
- `extra_files` adds files to the checksum file without uploading them; ensure they exist at the path specified.

## Republish / update behavior

Not applicable — this is a local packaging stage, not a publisher.

## Checksum config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name_template` | string | `{{ ProjectName }}_{{ Version }}_checksums.txt` | Checksum filename |
| `algorithm` | string | `sha256` | Hash algorithm |
| `disable` | bool | `false` | Disable checksum generation |
| `extra_files` | list | none | Additional files to checksum |
| `ids` | list | none | Only checksum artifacts matching these IDs |

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
