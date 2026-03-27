+++
title = "Checksums"
description = "Generate cryptographic checksums for all artifacts"
weight = 2
template = "docs.html"
+++

The checksum stage computes cryptographic hashes for all artifacts and writes them to a checksum file.

## Minimal config

Checksums are enabled by default with SHA-256. No config needed for basic usage.

## Checksum config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name_template` | string | `{{ ProjectName }}-{{ Version }}-checksums.txt` | Checksum filename |
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

## Custom config

```yaml
defaults:
  checksum:
    name_template: "{{ ProjectName }}-{{ Version }}-SHA512SUMS"
    algorithm: sha512
```

## Disabling checksums

```yaml
defaults:
  checksum:
    disable: true
```
