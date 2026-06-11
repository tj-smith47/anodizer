+++
title = "Post-release verification"
description = "Opt-in gate that REPORTS post-publish defects — missing assets, failed install smoke-tests, glibc-ceiling violations"
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

Three independently-toggleable checks:

| Check | What it catches | Needs |
|---|---|---|
| **asset-existence** | A produced artifact that never made it onto the published release (the partial uploads GitHub silently tolerates) | network |
| **install smoke-test** | A `.deb` / `.rpm` / `.apk` that won't install or whose binary won't run `--version` | Docker |
| **libc ceiling** | A glibc-linked `.deb` that requires a glibc newer than your support floor | — |

## Minimal config

```yaml
verify_release:
  enabled: true
```

With just `enabled: true`, asset-existence runs (it needs no extra config —
anodizer already knows what it produced and can fetch the release's asset
list). The smoke-test and libc-ceiling checks stay off until you configure
them.

## Full config reference

```yaml
verify_release:
  enabled: true            # default false — the whole gate is opt-in
  assert_assets: true      # default true — diff produced vs. uploaded assets
  install_smoke:           # absent => smoke-test off
    deb: { image: "debian:12" }      # default debian:stable-slim
    rpm: { image: "fedora:40" }      # default fedora:latest
    apk: { image: "alpine:3.20" }    # default alpine:latest
  glibc_ceiling: "2.36"    # absent => libc check off
```

## (a) asset-existence

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

## (b) install smoke-test

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

## (c) libc ceiling

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
  the smoke-test; the asset and libc checks need neither Docker nor extra
  config.

## Workspaces

In workspace modes (lockstep or per-crate), the gate verifies **every** published
crate: asset-existence across each crate's produced set, install-smoke and
libc-ceiling across each crate's Linux packages. No crate is siloed.

## Skipping

```bash
anodizer release --skip=verify-release
```
