+++
title = "Build Provenance / Attestations"
description = "SLSA build-provenance for binaries and archives, in subjects (OIDC) or emit (self-contained) mode"
weight = 4
template = "docs.html"
+++

The attestation stage produces SLSA build-provenance for your binaries and
archives. It runs after `checksum` (so subject digests reuse the sha256 the
checksum stage already computed) and before `sign`.

## Classification

Integrity — supply-chain metadata alongside release artifacts. Required: not a
publisher; a no-op unless `attestations.enabled` is `true`.

## Two modes

GitHub's [`actions/attest-build-provenance`](https://github.com/actions/attest-build-provenance)
is OIDC-bound to the Actions run, so anodizer cannot mint a GitHub-trusted
attestation itself. The `mode:` field selects how anodizer participates.

| Mode | Who attests | Trust | Output |
|---|---|---|---|
| `subjects` (default) | `actions/attest-build-provenance` (OIDC) | GitHub-trusted | `dist/attestation-subjects.json` |
| `emit` | anodizer + your sign key | keyed (weaker) | `attestation.intoto.jsonl` (signed, uploaded) |

## Configuration

```yaml
attestations:
  enabled: true
  mode: subjects          # or: emit ; default = subjects
  artifacts: [archive, binary, checksum]
```

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable the stage. When false, no-op. |
| `mode` | `subjects` \| `emit` | `subjects` | Participation mode (see below). |
| `artifacts` | list | `[archive, binary, checksum]` | Which produced-artifact **kinds** to attest. The concrete subject set (filenames + sha256) is derived from the artifacts anodizer already produced — never hand-listed. |
| `skip` | bool \| template | — | Skip the stage (also `--skip=attest`). |

`artifacts` selects KINDS, not files:

- `archive` — packaged archives (`.tar.gz`, `.zip`) and self-extracting archives
- `binary` — raw uploadable binaries
- `checksum` — checksum files (`checksums.txt` and split sidecars)

## Mode `subjects` (default) — the OIDC path

anodizer writes a **subjects manifest** that
[`anodizer-action`](https://github.com/toss45/anodizer-action) feeds to
`actions/attest-build-provenance`. anodizer does NOT attest itself in this mode;
the Action's OIDC identity does. This is the path used by fd, biome, and gping.

`dist/attestation-subjects.json` is an array of subjects:

```json
[
  {
    "name": "myapp-1.0.0-linux-amd64.tar.gz",
    "digest": { "sha256": "9f86d0818..." }
  },
  {
    "name": "myapp_1.0.0_checksums.txt",
    "digest": { "sha256": "2c26b46b6..." }
  }
]
```

### How the manifest is consumed

The JSON manifest is INPUT to **anodizer-action's own code**, which iterates the
entries and fans each out to the stock action via `subject-name` +
`subject-digest` (one subject per `{ name, digest.sha256 }` pair):

```yaml
# anodizer-action reads dist/attestation-subjects.json and, per entry, calls:
- uses: actions/attest-build-provenance@v1
  with:
    subject-name: myapp-1.0.0-linux-amd64.tar.gz
    subject-digest: sha256:9f86d0818...
```

> **Do not** point the stock action's `subject-path:` at
> `attestation-subjects.json`. `subject-path` expects the ACTUAL artifact files
> (the action hashes them itself); aiming it at the manifest would attest the
> JSON file's own hash, not your artifacts.

### Consuming directly with the stock action

If you drive `actions/attest-build-provenance` yourself (without
anodizer-action), the recommended path is `subject-checksums` pointed at the
checksum file anodizer's `checksum` stage already writes:

```yaml
# Recommended: subject-checksums reads the existing checksums.txt
# (sha256sum format `<hex>  <name>`); no extra file is generated.
- uses: actions/attest-build-provenance@v1
  with:
    subject-checksums: dist/checksums.txt
```

Alternatively, point `subject-path` at the real artifact globs (NOT the
manifest) so the action hashes the files itself:

```yaml
- uses: actions/attest-build-provenance@v1
  with:
    subject-path: "dist/*.tar.gz,dist/checksums.txt"
```

anodizer does **not** duplicate the sha256sum file: `checksums.txt` is in
`<hex>  <name>` format, which `subject-checksums` accepts directly. The JSON
manifest is the primary deliverable for anodizer-action; reusing
`checksums.txt` for the stock-action path is a zero-cost bonus.

## Mode `emit` — self-contained (GoReleaser Pro parity)

For users who can't run the Action. anodizer generates an
[in-toto v1](https://in-toto.io/Statement/v1) statement carrying an
[SLSA provenance v1](https://slsa.dev/provenance/v1) predicate over the selected
artifacts and writes it to `dist/attestation.intoto.jsonl`:

```json
{
  "_type": "https://in-toto.io/Statement/v1",
  "subject": [
    { "name": "myapp-1.0.0-linux-amd64.tar.gz", "digest": { "sha256": "9f86d0818..." } }
  ],
  "predicateType": "https://slsa.dev/provenance/v1",
  "predicate": {
    "buildDefinition": {
      "buildType": "https://anodizer.dev/release/v1",
      "externalParameters": { "tag": "v1.0.0", "version": "1.0.0" },
      "internalParameters": {},
      "resolvedDependencies": []
    },
    "runDetails": {
      "builder": { "id": "https://anodizer.dev" },
      "metadata": { "invocationId": "v1.0.0" }
    }
  }
}
```

The statement is registered as an uploadable artifact, so:

1. The existing [`signs:`](@/docs/sign/binaries-archives.md) stage signs it
   (cosign/gpg) — anodizer adds no new signing path; the statement rides the
   same `signs:` loop as every other artifact when `artifacts: all`.
2. The `release` stage uploads `attestation.intoto.jsonl` (and its signature)
   as a release asset.

To sign the emit-mode statement, configure a sign block that covers all
artifacts:

```yaml
attestations:
  enabled: true
  mode: emit
signs:
  - artifacts: all      # signs the .intoto.jsonl alongside archives/checksums
    cmd: cosign
```

This mode is keyed (not OIDC) and carries weaker trust than the Action path.

## Determinism

The in-toto statement omits the optional `startedOn` / `finishedOn`
timestamps so two retries of the same tag produce byte-identical statement
bytes — a re-uploaded asset never trips GitHub's `already_exists` size check.

## Workspaces

In workspace per-crate mode, each published crate's output is written under a
crate-prefixed name so they don't clobber:

```text
dist/alpha.attestation-subjects.json
dist/beta.attestation-subjects.json
dist/alpha.attestation.intoto.jsonl    # emit mode
dist/beta.attestation.intoto.jsonl
```

Each manifest/statement covers only its own crate's artifacts.
