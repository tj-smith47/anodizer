+++
title = "Podman backend"
description = "Build and push images via podman instead of docker buildx (Linux-only)"
weight = 5
template = "docs.html"
+++

Anodizer supports the `podman` backend as a swap-in alternative to `docker buildx` for both `dockers_v2[]` image builds and `docker_manifests[]` manifest-list publication. Matches GoReleaser Pro's `podman` pipe parity.

## Linux-only

The podman backend is **Linux-only**, matching GoReleaser Pro. Anodizer refuses to load a config with `use: podman` on macOS or Windows hosts and surfaces a clear error rather than failing later with `podman: command not found`.

```text
$ anodizer release         # on macOS
Error: podman backend is supported on Linux only (host OS: macos);
       remove `use: podman` or run on a Linux host
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
Error: docker_v2 with `use: podman` is incompatible with buildx-only flag
       '--cache-from=type=gha'; remove the flag or switch to `use: buildx`
```

Bare `--build-arg`, `--label`, `--platform`, `--tag`, `--no-cache`, and `--iidfile` are accepted on both backends.

## Image digest

`podman build` cannot bake `--push` into the build, so anodizer publishes each rendered tag afterwards with `podman push` (single-platform) or `podman manifest push --all` (multi-platform). Both carry `--digestfile`, which writes the digest the destination registry now holds — the image manifest for a single-platform push, the manifest list for a `manifest push`. That is the same kind of value buildx reports as `containerimage.digest`, so `{{ Digest }}` in a `post:` hook, the `<tag>.digest` artifact and the release's docker landing check behave identically on both backends.

Measured against a local `registry:2` under podman 5.8.4, both verbs wrote exactly the digest the registry then served:

```text
$ podman push --tls-verify=false --digestfile=/out/push.digest localhost:5000/probe:t
$ cat /out/push.digest
sha256:175760276794f3dcc233eda09300ed71cf1bca74add010530480869e7b370b8e
$ curl -sI http://localhost:5000/v2/probe/manifests/t | grep -i docker-content-digest
Docker-Content-Digest: sha256:175760276794f3dcc233eda09300ed71cf1bca74add010530480869e7b370b8e

$ podman manifest push --tls-verify=false --digestfile=/out/manifest.digest probelist docker://localhost:5000/probelist:t
$ cat /out/manifest.digest
sha256:d45fc543f733090503ea990e92e6a313195925fd96ed63947f9501549e7d8bd3
$ curl -sI http://localhost:5000/v2/probelist/manifests/t | grep -i docker-content-digest
Docker-Content-Digest: sha256:d45fc543f733090503ea990e92e6a313195925fd96ed63947f9501549e7d8bd3
```

**Podman 2.0 is the floor for the podman backend.** `podman push` took `--digestfile` in 1.6.0 — "The `podman push` command now supports the `--digestfile` option to save a file containing the pushed digest" ([RELEASE_NOTES.md, 1.6.0 Features](https://github.com/containers/podman/blob/main/RELEASE_NOTES.md)) — and `podman manifest push` in 2.0.0, the first tag whose [`podman-manifest-push.1.md`](https://github.com/containers/podman/blob/v2.0.0/docs/source/markdown/podman-manifest-push.1.md) documents it; the page carries no such flag at 1.9.0. The higher of the two is the floor. An older podman exits non-zero on the unknown option, which fails the push rather than degrading to a build with no digest. A podman that accepts the flag but writes nothing degrades cleanly: anodizer notes the missing file under `-v` and records no digest for that tag.

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
