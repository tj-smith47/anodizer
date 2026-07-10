+++
title = "Post-release verification"
description = "Opt-in gate that REPORTS post-publish defects — missing or corrupted assets, unlanded publishes, failed install smoke-tests, glibc-ceiling violations"
weight = 56
template = "docs.html"
+++

The `verify_release:` gate runs **last** in the release pipeline — after the
release is created and every publisher has run — and **reports** post-publish
defects. Because it runs *after* the irreversible publish, it never blocks or
undoes anything: a failed check surfaces the problem and exits non-zero so CI
flags it, but **the release is already published**.

It is distinct from per-publisher [`post_publish_poll`](../../publish/) (which
waits on Chocolatey/WinGet moderation queues): `verify_release` is a broader,
top-level gate covering the GitHub release's assets and the produced Linux
packages.

## What it checks

Four independently-toggleable checks:

| Check | What it catches | Needs |
|---|---|---|
| **asset existence + content** | A produced artifact that never made it onto the published release (the partial uploads GitHub silently tolerates), an uploaded asset whose **size or sha256 digest** doesn't match the local bytes (truncated/corrupted uploads, stale assets from a prior re-cut), **and** a signature / SBOM asset your `signs:` / `sboms:` config demands that was never produced at all (a silently no-op'd sign or SBOM stage) | network |
| **publisher landing checks** | A publisher that reported success without the artifact actually landing: a crate version missing from the crates.io index, an npm version the registry doesn't serve, a blob object absent from its bucket, a snap held for manual store review and live in no channel | network |
| **install smoke-test** | A `.deb` / `.rpm` / `.apk` that won't install or whose binary won't run `--version` | Docker |
| **libc ceiling** | A glibc-linked `.deb` that requires a glibc newer than your support floor | — |

## Minimal config

```yaml
verify_release:
  enabled: true
```

With just `enabled: true`, the asset check and the landing checks run (they
need no extra config — anodizer already knows what it produced, can fetch the
release's asset list, and the run's own publish report carries every landing
coordinate). The smoke-test and libc-ceiling checks stay off until you
configure them.

## Full config reference

```yaml
verify_release:
  enabled: true            # default false — the whole gate is opt-in
  assert_assets: true      # default true — diff produced vs. uploaded assets + size/digest
  assert_landing: true     # default true — probe cargo/npm/blob landings
  install_smoke:           # absent => smoke-test off
    deb: { image: "debian:12" }      # default debian:stable-slim
    rpm: { image: "fedora:40" }      # default fedora:latest
    apk: { image: "alpine:3.20" }    # default alpine:latest
  glibc_ceiling: "2.36"    # absent => libc check off
```

## (a) asset existence + content

```yaml
verify_release:
  enabled: true
  assert_assets: true
```

anodizer diffs the artifacts it **produced** (the same upload set the release
stage uploads from) against the assets actually **stored** on the published
GitHub release, then reports any produced artifact with no matching uploaded
asset:

```
$ anodizer release
...
[verify-release] crate 'myapp': 1 produced artifact(s) missing from the published
                 release: myapp_1.0.0_amd64.deb
Error: verify-release: post-publish verification found 1 issue(s);
       the release IS published — investigate:
  - crate 'myapp': 1 produced artifact(s) missing from the published release: myapp_1.0.0_amd64.deb
```

Extra assets on the release (orphans from a prior re-cut) are reported as an
advisory, never a failure on their own.

### Size + digest verification

Every expected asset that **is** present also gets a byte-level check: the
stored size must equal the local artifact's size, and the stored sha256 digest
(GitHub computes one server-side for every uploaded asset) must equal the
local sha256 — the checksum stage's already-computed hash is reused when
available. A clean pass emits one result line:

```
[verify-release] github: crate 'myapp' 22/22 assets present, sizes+digests match
```

A mismatch names the asset and both values:

```
- asset 'myapp_1.0.0_linux_amd64.tar.gz' of crate 'myapp' size mismatch: local 4194304 B vs
  published 1048576 B — the uploaded asset does not match the produced artifact
```

When the release serves no digest for an asset (older GitHub Enterprise),
anodizer downloads the asset and hashes it — capped at 64 MiB per asset;
beyond the cap the asset is verified by size only, with a verbose notice.

### Config-derived signature / SBOM expectations

The produced set alone cannot catch a sign or SBOM stage that silently
produced **nothing** — there is no registered artifact to miss. So the check
additionally derives, from your resolved config plus the artifact set, the
signature / certificate / SBOM asset names that **should** exist:

- each `signs:` entry contributes one signature (and one certificate, when
  `certificate:` is set) per artifact its `artifacts:`/`ids:` filters select,
  named by its `signature:` template;
- each `sboms:` entry contributes its rendered `documents:` names per matched
  artifact.

No new config is required — the expectations come from what anodizer already
knows. A release missing them fails with the exact names:

```
- crate 'myapp': 2 signature/SBOM asset(s) required by the resolved signs/sboms
  config were never uploaded (the producing stage registered no such artifact):
  myapp_1.0.0_checksums.txt.sig, myapp_1.0.0_linux_amd64.tar.gz.cdx.json
```

Intentional skips create **no** expectations: a sign config whose `if:`
evaluated falsy or whose `artifacts: none` disabled it (the run's own skip
record is consulted first as the authoritative account of what this run
decided), an SBOM config whose `skip:` evaluated truthy, or a whole stage
skipped via `--skip=sign` / `--skip=sbom`. SBOM `documents:` containing glob
patterns are not predictable from config and create no expectations either.
Under a `release.ids` upload filter, expectations follow the SUBJECT's
verdict — a signature or SBOM is expected exactly when the artifact it
derives from is uploaded.

## (b) publisher landing checks

```yaml
verify_release:
  enabled: true
  assert_landing: true   # the default
```

Every publisher that **succeeded this run** is probed to confirm the publish
actually landed — using the coordinates the run's own publish report recorded,
so no extra config is needed:

| Publisher | Probe |
|---|---|
| `cargo` | crates.io **sparse index** lookup for every published `crate@version` (custom `registry:`/`index:` targets are skipped — the crates.io index says nothing about them) |
| `npm` | registry metadata `GET <registry>/<pkg>/<version>` for every published package |
| `blob` | `HEAD` on every uploaded object, through the **same store backend and ambient credentials** the upload used — works for private buckets with no public URL |
| `snapcraft` | anonymous `GET api.snapcraft.io/v2/snaps/info/<snap>` for every uploaded snap — the version must be **live in the store's channel map** (in the released channel when one was set). This catches the Snap Store's silent failure mode: a manual-review hold accepts the upload but ships nothing until a human approves, and a decline arrives only by email |

One result line per publisher:

```
[verify-release] cargo: anodizer-core@0.15.4 visible on crates.io index
[verify-release] npm: myapp@0.15.4 visible on registry.npmjs.org
[verify-release] blob: 22/22 uploaded object(s) present in bucket
[verify-release] snapcraft: myapp 1.0.0 live in the Snap Store channel map
```

A publisher that was skipped, deselected, or failed is not probed — it landed
nothing this run. A probe that **cannot run** (index unreachable, store build
failure) is reported as an issue, never silently passed: an unverifiable
landing is a finding.

```
- cargo: myapp@1.0.0 reported published but is not visible on the crates.io index
- blob: s3://my-bucket/v1.0.0/myapp.tar.gz reported uploaded but is missing from the bucket
- snapcraft: myapp 1.0.0 was HELD for Snap Store manual review and is not live in the store — consumers get nothing until review approves (https://dashboard.snapcraft.io/snaps/myapp/)
```

## (c) install smoke-test

```yaml
verify_release:
  enabled: true
  install_smoke:
    deb: { image: "debian:12" }
    rpm: { image: "fedora:40" }
    apk: { image: "alpine:3.20" }
```

For each produced Linux package, anodizer runs the install + a version check in
a fresh pinned container, with the container platform pinned to the package's
**build architecture**:

```bash
docker run --rm --platform linux/arm64 \
  --mount type=bind,source=<pkg>,destination=/pkg/<pkg>,readonly debian:12 \
  sh -c "dpkg -i '/pkg/<pkg>' || (apt-get update && apt-get -y -f install) && 'myapp' --version"
```

Per-package install commands: `.deb` → `dpkg -i` (with an `apt-get -f` dependency
fixup), `.rpm` → `rpm -i --nodeps`, `.apk` → `apk add --allow-untrusted`. Each
image defaults to a sane base (`debian:stable-slim`, `fedora:latest`,
`alpine:latest`); override only the ones you care about.

The `--platform` pin comes from the package's build target — an arm64 `.deb`
is installed in an arm64 container, never the runner's native one. Running a
non-native platform needs cross-arch emulation (qemu/binfmt) on the Docker
host; anodizer probes for it once per platform and, when it is missing,
**fails that package's smoke-test loudly** instead of reporting a misleading
in-container arch error or silently skipping the coverage:

```
- crate 'myapp': install smoke-test failed for myapp_1.0.0_linux_arm64.deb on debian:stable-slim:
  cannot run linux/arm64 containers on this linux/amd64 host: cross-arch emulation (qemu/binfmt)
  is unavailable. Install it (e.g. `docker run --privileged --rm tonistiigi/binfmt --install all`)
  or run install_smoke on a linux/arm64 runner. The package was NOT smoke-tested.
```

When **Docker is unavailable**, the smoke-test is **skipped with a notice** —
it does not hard-fail the gate, and asset-existence and libc-ceiling still run:

```
[verify-release] Docker unavailable — skipping install smoke-test (asset-existence and libc-ceiling still run)
```

## (d) libc ceiling

```yaml
verify_release:
  enabled: true
  glibc_ceiling: "2.36"
```

anodizer reads the required glibc symbol versions from each glibc-linked
`.deb`'s embedded ELF (`.gnu.version_r`) and fails if the **maximum** required
version exceeds your floor — catching the classic "built on a too-new builder,
won't run on the target distro" regression:

```
Error: verify-release: post-publish verification found 1 issue(s);
       the release IS published — investigate:
  - crate 'myapp': usr/bin/myapp requires glibc 2.38, exceeding the configured ceiling 2.36
```

The comparison is **numeric, component-wise** (so `2.36 > 2.4`, not the wrong
lexical ordering). **musl** binaries have no glibc requirement and are
**skipped** — which is the whole point: a musl build would otherwise hide a
glibc-floor regression, so a ceiling-checked release should ship glibc `.deb`s
to be meaningful.

`glibc_ceiling` absent → the libc check is off.

## Failure semantics

- A detected defect is reported with the specific artifact / package / version,
  and the gate exits **non-zero** so CI fails the job.
- The wording is always explicit: **the release IS published** — the gate never
  implies the publish failed and never attempts to undo it.
- Each check is **best-effort and independent**: Docker-unavailable skips only
  the smoke-test; the asset, landing, and libc checks need neither Docker nor
  extra config.
- Check axes are gated on **their own** publisher's selection: a
  `--publishers npm` run still verifies the npm landing while skipping the
  GitHub asset check; only a run that selects none of `github-release`,
  `cargo`, `npm`, `blob` self-skips the whole gate.

## Workspaces

In workspace modes (lockstep or per-crate), the gate verifies **every** published
crate: asset-existence across each crate's produced set, install-smoke and
libc-ceiling across each crate's Linux packages. No crate is siloed.

## Skipping

```bash
anodizer release --skip=verify-release
```
