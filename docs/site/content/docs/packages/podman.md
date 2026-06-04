+++
title = "Podman backend"
description = "Build and push images via podman instead of docker buildx (Linux-only)"
weight = 5
template = "docs.html"
+++

Anodizer supports the `podman` backend as a swap-in alternative to `docker buildx` for both `docker_v2[]` image builds and `docker_manifests[]` manifest-list publication. Matches GoReleaser Pro's `podman` pipe parity.

## Linux-only

The podman backend is **Linux-only**, matching GoReleaser Pro. Anodizer refuses to load a config with `use: podman` on macOS or Windows hosts and surfaces a clear error rather than failing later with `podman: command not found`.

```text
$ anodize release         # on macOS
Error: podman backend is supported on Linux only (host OS: macos);
       remove `use: podman` or run on a Linux host
```

## Opt in

Set `use: podman` on a `docker_v2[]` entry (build path) or a `docker_manifests[]` entry (manifest-list path):

```yaml
crates:
  - name: myapp
    docker_v2:
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

## What anodizer does NOT do

Mirrors GoReleaser Pro's caveats verbatim:

- **No auto-install.** Anodizer never installs `podman` for you. CI runners must have `podman` already on `PATH`.
- **No credential setup.** Push credentials are resolved from the host's `~/.docker/config.json` (or `~/.config/containers/auth.json` for rootless podman). Run `podman login` (or `docker login`) before releasing, or wire `DOCKER_USERNAME` / `DOCKER_PASSWORD` into a `before:` hook.
- **No rootless / rootful opinion.** Anodizer treats the binary as opaque — whether `podman` runs rootless (default on most distros) or rootful is your runner's choice. Image layers and manifests written under rootless are stored at `$XDG_DATA_HOME/containers/storage`; rootful at `/var/lib/containers/storage`.
- **No network reach checks.** Push failures retry per the `retry:` block (default 10 attempts, 10s base, 5m cap).

## Determinism harness compatibility

`anodize check determinism` shells out to `docker buildx build --output=type=oci,rewrite-timestamp=true,...` for its byte-stability probe. Those flags are BuildKit-only and have no podman equivalent. When the project config has `use: podman` set on any `docker_v2[]` entry, the harness skips the docker stage with an explanatory warning:

```text
warn: docker stage requested but project config has `use: podman` (Linux-only);
      the determinism harness only probes BuildKit-based builds, so the docker
      stage is skipped for this run. Verify podman image byte-stability outside
      the harness.
```

Verify podman image reproducibility out-of-band (re-build the image twice, compare the layer-tar hashes).

## Healthcheck

`anodize healthcheck` probes `podman --version` alongside `docker --version` so operators can confirm the binary is reachable before opting into the backend.

## Anodizer extension: `docker_manifests[].use: podman`

GoReleaser's `validateManifester` only accepts `use: docker` on `docker_manifests[]` entries. Anodizer extends the set to include `podman` because `podman manifest create / push` mirrors `docker manifest`. This is an intentional anodizer-only extension — GR-imported configs that already set `use: podman` here continue to work; new configs should be aware they are stepping outside strict parity.
