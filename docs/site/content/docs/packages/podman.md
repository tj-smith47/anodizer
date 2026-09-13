+++
title = "Podman backend"
description = "Build and push images via podman instead of docker buildx (Linux-only)"
weight = 5
template = "docs.html"
+++

Anodizer supports the `podman` backend as a swap-in alternative to `docker buildx` for both `dockers_v2[]` image builds and `docker_manifests[]` manifest-list publication. Matches GoReleaser Pro's `podman` pipe parity.

## Linux-only

The podman backend is **Linux-only**, matching GoReleaser Pro. Anodizer refuses to load a config with `use: podman` on macOS or Windows hosts and reports a clear error rather than failing later with `podman: command not found`.

```text
$ anodizer release         # on macOS
       Error podman backend is supported on Linux only (host OS: macos); remove `use: podman` or run on a Linux host
```

## Opt in

Set `use: podman` on a `dockers_v2[]` entry (build path) or a `docker_manifests[]` entry (manifest-list path):

```yaml
crates:
  - name: myapp
    dockers_v2:
      - id: app-podman
        images: ["ghcr.io/myorg/myapp"]
        tags: ["{{ Version }}", "latest"]
        dockerfile: Dockerfile
        platforms: ["linux/amd64", "linux/arm64"]
        use: podman           # opt in (Linux-only)
        sbom: false           # MUST be false under podman
    docker_manifests:
      - name_template: "ghcr.io/myorg/myapp:{{ Version }}"
        image_templates:
          - "ghcr.io/myorg/myapp:{{ Version }}-amd64"
          - "ghcr.io/myorg/myapp:{{ Version }}-arm64"
        use: podman
```

The default (`use:` unset) keeps the historical `docker buildx` invocation.

## Flag compatibility

Plain `podman build` does **not** accept the BuildKit-only flag set. Anodizer rejects configs that mix `use: podman` with any of:

| Flag | Why rejected |
|------|--------------|
| `--rewrite-timestamp` | BuildKit-only deterministic-mtime exporter attribute |
| `--sbom` | BuildKit SBOM attestation; not in podman |
| `--provenance` | BuildKit in-toto provenance attestation |
| `--attest` | BuildKit attestation umbrella |
| `--output` | BuildKit exporter selector (OCI / registry / image) |
| `--cache-from` | BuildKit cache importer |
| `--cache-to` | BuildKit cache exporter |
| `sbom: true` | Resolves to `--attest=type=sbom`; same reason |

```text
       Error dockers_v2 with `use: podman` is incompatible with buildx-only flag '--cache-from=type=gha'; remove the flag or switch to `use: buildx`
```

Bare `--build-arg`, `--label`, `--platform`, `--tag`, `--no-cache`, and `--iidfile` are accepted on both backends.

## Image digest

`podman build` cannot bake `--push` into the build, so anodizer publishes each rendered tag afterwards with `podman push` (single-platform) or `podman manifest push --all` (multi-platform). Both carry `--digestfile`, which writes the digest the destination registry now holds — the image manifest for a single-platform push, the manifest list for a `manifest push`. That is the same kind of value buildx reports as `containerimage.digest`, so `{{ Digest }}` in a `post:` hook, the `<tag>.digest` artifact and the release's docker landing check behave identically on both backends.

Measured against a local `registry:2` under podman 5.8.4, both verbs wrote exactly the digest the registry then served. The measurement is a script — `crates/stage-docker/tests/data/podman-digestfile-measure.sh` — and the transcript beside it (`podman-digestfile-vs-registry.txt`) is that script's stdout, so the commands below are the ones that produced the output below:

```text
$ podman push --tls-verify=false --digestfile=/out/push.digest localhost:5000/probe:t
Getting image source signatures
Copying blob sha256:4e7bc3f990a0afcd6a47cb46a70f4141418bec70a9bbf812230c4a9ccd5dd723
Copying config sha256:584cf49a13f6d372df12420839e289c39286bc37149fc46aca9be18057c344e0
Writing manifest to image destination
$ cat /out/push.digest
sha256:4fc24e873ce52bc5f8400cb780f6319c2abb1343c18627b3d49b607e874a26d2

$ curl -i -H 'Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json' \
    http://localhost:5000/v2/probe/manifests/t
HTTP/1.1 200 OK
Content-Length: 567
Content-Type: application/vnd.oci.image.manifest.v1+json
Docker-Content-Digest: sha256:4fc24e873ce52bc5f8400cb780f6319c2abb1343c18627b3d49b607e874a26d2
Docker-Distribution-Api-Version: registry/2.0
Etag: "sha256:4fc24e873ce52bc5f8400cb780f6319c2abb1343c18627b3d49b607e874a26d2"
X-Content-Type-Options: nosniff
Date: Sun, 13 Sep 2026 07:25:46 GMT

{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:584cf49a13f6d372df12420839e289c39286bc37149fc46aca9be18057c344e0","size":520},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:249002a2f534c7e6e72a81770ed64d602b47233810fd08586a1d703d77bf3fce","size":107}],"annotations":{"org.opencontainers.image.base.digest":"","org.opencontainers.image.base.name":"","org.opencontainers.image.created":"2026-09-13T07:25:45.410981776Z"}}

$ podman manifest create probelist
e52c2a31bd6236a7faa9b57b785bc6770b82c9725ccfe03f326cc5580f483476
$ podman manifest add probelist --tls-verify=false localhost:5000/probe:t
e52c2a31bd6236a7faa9b57b785bc6770b82c9725ccfe03f326cc5580f483476
$ podman manifest push --tls-verify=false --digestfile=/out/manifest.digest probelist docker://localhost:5000/probelist:t
Getting image list signatures
Copying 1 images generated from 1 images in list
Copying image sha256:4fc24e873ce52bc5f8400cb780f6319c2abb1343c18627b3d49b607e874a26d2 (1/1)
Getting image source signatures
Copying blob sha256:249002a2f534c7e6e72a81770ed64d602b47233810fd08586a1d703d77bf3fce
Copying config sha256:584cf49a13f6d372df12420839e289c39286bc37149fc46aca9be18057c344e0
Writing manifest to image destination
Writing manifest list to image destination
Storing list signatures
$ cat /out/manifest.digest
sha256:ac03ebfa3da3648ff63c2d5544f519f67d76eb0a64c918086a6dd6460b3866f8

$ curl -i -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json' \
    http://localhost:5000/v2/probelist/manifests/t
HTTP/1.1 200 OK
Content-Length: 289
Content-Type: application/vnd.oci.image.index.v1+json
Docker-Content-Digest: sha256:ac03ebfa3da3648ff63c2d5544f519f67d76eb0a64c918086a6dd6460b3866f8
Docker-Distribution-Api-Version: registry/2.0
Etag: "sha256:ac03ebfa3da3648ff63c2d5544f519f67d76eb0a64c918086a6dd6460b3866f8"
X-Content-Type-Options: nosniff
Date: Sun, 13 Sep 2026 07:25:46 GMT

{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:4fc24e873ce52bc5f8400cb780f6319c2abb1343c18627b3d49b607e874a26d2","size":567,"platform":{"architecture":"amd64","os":"linux"}}]}
```

**Podman 2.0 is the floor for the podman backend.** `podman push` took `--digestfile` in 1.6.0 — "The `podman push` command now supports the `--digestfile` option to save a file containing the pushed digest" ([RELEASE_NOTES.md at v1.6.0, 1.6.0 Features](https://github.com/podman-container-tools/podman/blob/v1.6.0/RELEASE_NOTES.md)) — and `podman manifest push` did not exist before 2.0.0: v1.9.0 carries no `podman-manifest-*` man page at all, and v2.0.0's [`podman-manifest-push.1.md`](https://github.com/podman-container-tools/podman/blob/v2.0.0/docs/source/markdown/podman-manifest-push.1.md) documents the flag from the start. The higher of the two is the floor. An older podman exits non-zero on the unknown option, which fails the push rather than degrading to a build with no digest. A podman that accepts the flag but writes nothing degrades cleanly: anodizer notes the missing file under `-v` and records no digest for that tag.

A build that pushes nothing (a snapshot, a dry run, `skip_push:`) records no digest under either backend — `podman build`'s own `--iidfile` holds the LOCAL image ID, which names different content than a registry serves.

## What anodizer does NOT do

Mirrors GoReleaser Pro's caveats verbatim:

- **No auto-install.** Anodizer never installs `podman` for you. CI runners must have `podman` already on `PATH`.
- **No credential setup.** Push credentials are resolved from the host's `~/.docker/config.json` (or `~/.config/containers/auth.json` for rootless podman). Run `podman login` (or `docker login`) before releasing, or wire `DOCKER_USERNAME` / `DOCKER_PASSWORD` into a `before:` hook.
- **No rootless / rootful opinion.** Anodizer treats the binary as opaque — whether `podman` runs rootless (default on most distros) or rootful is your runner's choice. Image layers and manifests written under rootless are stored at `$XDG_DATA_HOME/containers/storage`; rootful at `/var/lib/containers/storage`.
- **No network reach checks.** Push failures retry per the `retry:` block (default 10 attempts, 10s base, 5m cap).

## Determinism Harness compatibility

`anodizer check determinism` shells out to `docker buildx build --output=type=oci,rewrite-timestamp=true,...` for its byte-stability probe. Those flags are BuildKit-only and have no podman equivalent. When the project config has `use: podman` set on any `dockers_v2[]` entry, the harness skips the docker stage with an explanatory warning:

```text
warn: docker stage requested but project config has `use: podman` (Linux-only);
      the determinism harness only probes BuildKit-based builds, so the docker
      stage is skipped for this run. Verify podman image byte-stability outside
      the harness.
```

Verify podman image reproducibility out-of-band (re-build the image twice, compare the layer-tar hashes).

## Healthcheck

`anodizer healthcheck` probes `podman --version` alongside `docker --version` so operators can confirm the binary is reachable before opting into the backend.

## Anodizer extension: `docker_manifests[].use: podman`

GoReleaser's `validateManifester` only accepts `use: docker` on `docker_manifests[]` entries. Anodizer extends the set to include `podman` because `podman manifest create / push` mirrors `docker manifest`. This is an intentional anodizer-only extension — GR-imported configs that already set `use: podman` here continue to work; new configs should be aware they are stepping outside strict parity.
