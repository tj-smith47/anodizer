+++
title = "Nix"
description = "Publish Nix derivations to a nixpkgs-style repository"
weight = 76
template = "docs.html"
+++

The Nix publisher generates a Nix derivation expression for your release and commits it to a nixpkgs-style repository. It is configured under `publish.nix` within a crate's config.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | false | git revert + push | `GITHUB_TOKEN contents:write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## The `required:` field

Default: **`false`** — a Nix derivation push failure is logged but does not fail the release.

Set `required: true` to make the release exit non-zero if this publisher fails:

```yaml
crates:
  - name: myapp
    publish:
      nix:
        repository:
          owner: my-org
          name: nixpkgs
        description: "A fast CLI tool"
        license: mit
        required: true
```

See [Publish overview — the `required:` field](../) for the full semantics.

## Minimal config

`description` and `license` are derived from the crate's `Cargo.toml`
`[package]` (`description` / `license`), so a typical Rust project supplies
only the repository:

```yaml
crates:
  - name: myapp
    publish:
      nix:
        repository:
          owner: my-org
          name: nixpkgs
```

Set `description` / `license` explicitly only to override the Cargo-derived
value (see [License identifiers](#license-identifiers) for the accepted
forms).

## Full config reference

```yaml
crates:
  - name: myapp
    publish:
      nix:
        name: myapp                          # optional; derivation pname (default: crate name)
        path: pkgs/myapp/default.nix         # optional; output path in the repo
        description: ""                      # optional; meta.description (derived from Cargo.toml)
        homepage: ""                         # optional; meta.homepage (derived from Cargo.toml)
        license: mit                         # optional; lib.licenses attr OR SPDX id; derived from Cargo SPDX if omitted
        ids: []                              # optional; filter by build IDs
        url_template: ""                     # optional; override download URL
        skip_upload: false                   # optional; "auto" skips prereleases
        install: ""                          # optional; custom install commands
        extra_install: ""                    # optional; appended after main install
        post_install: ""                     # optional; postInstall phase commands
        formatter: ""                        # optional; alejandra | nixfmt
        commit_msg_template: ""             # optional
        commit_author:
          name: ""
          email: ""
        repository:
          owner: my-org                      # optional; inferred if omitted
          name: nixpkgs                      # optional
          branch: main                       # optional
          token: "{{ Env.GITHUB_TOKEN }}"   # optional
          pull_request:
            enabled: false                   # optional; open PR instead of direct push
            base: master                     # optional
        dependencies:                        # optional; runtime deps as Nix attr paths
          - name: openssl                    # Nix attribute path
          - name: libiconv
            os: darwin                       # optional; restrict to linux | darwin
```

## Authentication

| Variable | Description |
|----------|-------------|
| `GITHUB_TOKEN` | Token with push access to your nixpkgs-style repository |

The token can also be set via `repository.token` in the config.

## Common gotchas

- **Linux/macOS only**: only Linux and Darwin artifacts are included. Windows artifacts are ignored.
- **License identifier**: `license` accepts either a `lib.licenses` attribute (`mit`, `asl20`) or an SPDX id (`MIT`, `Apache-2.0`) — anodizer maps the SPDX form to the nix attribute. Omit it and it derives from your `Cargo.toml` SPDX `license`. A compound SPDX expression (`MIT OR Apache-2.0`) has no single nix attribute, so it must be set explicitly to one `lib.licenses` attribute. Run `anodizer check` to validate before releasing.
- **SHA256 format**: checksums are automatically converted from hex to Nix SRI format (`sha256-<base64>`). Do not manually convert.
- **`formatter`**: if `alejandra` or `nixfmt` is set but not on `PATH`, the derivation is written without formatting (no error). Ensure the formatter is available in CI.

## Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | crate name | Override the derivation `pname`. |
| `path` | string | `pkgs/<name>/default.nix` | Output path for the `.nix` file in the repository. |
| `repository` | object | | Target repository config. See [Repository config](#repository-config). |
| `commit_author` | object | | Git commit author for the derivation commit. |
| `commit_msg_template` | string | | Custom commit message template. |
| `ids` | list | all | Filter to artifacts with these IDs. |
| `url_template` | string | | Override download URL template (defaults to release asset URL). |
| `skip_upload` | string | | `"true"` always skips; `"auto"` skips for prereleases. |
| `install` | string | | Custom install commands replacing auto-generated binary install. |
| `extra_install` | string | | Additional install commands appended after the main install. |
| `post_install` | string | | Commands for the `postInstall` phase. |
| `description` | string | | Short description for the derivation's `meta.description`. |
| `homepage` | string | | Project homepage URL for `meta.homepage`. |
| `license` | string | Cargo SPDX | `lib.licenses` attribute (`mit`, `asl20`) or an SPDX id (`MIT`, `Apache-2.0`); SPDX is mapped to the nix attribute. Derived from `Cargo.toml [package].license` when omitted. Compound SPDX requires an explicit single attribute. |
| `dependencies` | list | | Runtime dependencies as Nix package names. |
| `formatter` | string | | Nix formatter to run on the generated file: `alejandra` or `nixfmt`. |

### Repository config

```yaml
repository:
  owner: my-org
  name: nixpkgs
  branch: main           # target branch
  token: "{{ Env.GITHUB_TOKEN }}"
  pull_request:
    enabled: true        # open a PR instead of pushing directly
    base: master
```

### Dependencies

Dependencies are added to the derivation's `nativeBuildInputs`. When any dependency is present, `makeWrapper` is automatically added and a `wrapProgram` call is generated to extend `PATH`.

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Nix attribute path (e.g. `openssl`, `pkgs.libgit2`). |
| `os` | string | Restrict to `linux` or `darwin`. Empty means all platforms. |

```yaml
dependencies:
  - name: openssl
  - name: libiconv
    os: darwin
  - name: pkg-config
    os: linux
```

## Generated derivation format

The generated derivation uses `stdenvNoCC.mkDerivation` with `fetchurl`. The SHA256 hashes from release checksums are automatically converted from hex to Nix SRI format (`sha256-<base64>`).

```nix
{ lib
, stdenvNoCC
, fetchurl
, installShellFiles
}:

let
  selectSystem = attrs: attrs.${stdenvNoCC.hostPlatform.system} or (throw "Unsupported system: ${stdenvNoCC.hostPlatform.system}");
  urlMap = {
    x86_64-linux = "https://github.com/owner/repo/releases/download/v1.0.0/myapp_1.0.0_linux_amd64.tar.gz";
    aarch64-linux = "https://github.com/owner/repo/releases/download/v1.0.0/myapp_1.0.0_linux_arm64.tar.gz";
    x86_64-darwin = "https://github.com/owner/repo/releases/download/v1.0.0/myapp_1.0.0_darwin_amd64.tar.gz";
    aarch64-darwin = "https://github.com/owner/repo/releases/download/v1.0.0/myapp_1.0.0_darwin_arm64.tar.gz";
  };
  shaMap = {
    x86_64-linux = "sha256-abc123...";
    ...
  };
in
stdenvNoCC.mkDerivation {
  pname = "myapp";
  version = "1.0.0";

  src = fetchurl {
    url = selectSystem urlMap;
    sha256 = selectSystem shaMap;
  };

  sourceRoot = ".";

  nativeBuildInputs = [ installShellFiles ];

  installPhase = ''
    mkdir -p $out/bin
    cp -vr ./myapp $out/bin/myapp
  '';

  meta = {
    description = "A fast CLI tool";
    license = lib.licenses.mit;
    sourceProvenance = with lib.sourceTypes; [ binaryNativeCode ];
    platforms = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
  };
}
```

## Root `flake.nix`

Alongside each `pkgs/<name>/default.nix` derivation, the publisher writes (and keeps up to date) a root `flake.nix` so the repository is directly **flake-installable** — not just usable as an overlay. After a publish, consumers can install or run a package by name without cloning:

```bash
nix profile install github:<owner>/<repo>#<name>
nix build github:<owner>/<repo>#<name>
nix run   github:<owner>/<repo>#<name>
```

The flake pins `nixpkgs` (`nixos-unstable`) and exposes, for every published package:

- `packages.<system>.<name>` for `x86_64-linux`, `aarch64-linux`, `x86_64-darwin`, and `aarch64-darwin`.
- `overlays.default`, composing the same packages — so existing overlay consumers keep working.

```nix
{
  description = "Nix flake for release artifacts published by anodize";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ self.overlays.default ];
      };
    in
    {
      overlays.default = final: prev: {
        myapp = final.callPackage ./pkgs/myapp/default.nix { };
      };

      packages = forAllSystems (system:
        let pkgs = pkgsFor system;
        in {
          myapp = pkgs.myapp;
        });
    };
}
```

The flake is regenerated on every publish by **merging** the package just written into the set recovered from the previously committed `flake.nix`. This keeps it correct across all configurations:

- **Single crate** — one package, exposed for all four systems.
- **Workspace (lockstep)** — every crate's package is exposed at the shared version.
- **Workspace (per-crate versions)** — every crate's package is exposed at its own version and tag.
- **Custom `path`** — a derivation written to a non-default path (e.g. `nix/myapp.nix`) is exposed by its real path; siblings published at other paths are preserved (never clobbered).

The flake's top-level `description` is a fixed, repo-level string, so a multi-crate publish produces byte-stable output regardless of which crate published last.

## Platform mapping

Only Linux and macOS (Darwin) artifacts are included. Nix system strings are derived from the artifact's OS and architecture:

| OS / Arch | Nix system |
|-----------|------------|
| linux / amd64 | `x86_64-linux` |
| linux / arm64 | `aarch64-linux` |
| linux / 386 | `i686-linux` |
| darwin / amd64 | `x86_64-darwin` |
| darwin / arm64 | `aarch64-darwin` |

## License identifiers

`license` accepts either form, and anodizer normalizes it to a nix
`lib.licenses` attribute for the derivation:

- **`lib.licenses` attribute** — used verbatim. Common values: `mit`, `asl20`,
  `gpl3Only`, `gpl3Plus`, `lgpl21Only`, `mpl20`, `isc`, `bsd2`, `bsd3`,
  `unlicense`.
- **SPDX id** — e.g. `MIT`, `Apache-2.0`, `BSD-3-Clause` — mapped to the
  matching nix attribute (`MIT` → `mit`, `Apache-2.0` → `asl20`).

Omit `license` entirely and it derives from your `Cargo.toml`
`[package].license` (an SPDX expression), so a plain Rust crate needs no nix
`license` at all. A **compound** SPDX expression (`MIT OR Apache-2.0`,
`Apache-2.0 WITH LLVM-exception`) has no single `lib.licenses` attribute and
cannot be auto-mapped — set `nix.license` to one explicit attribute in that
case.

Run `anodizer check` to validate the resolved license identifier before
releasing.

## Skipping prereleases

```yaml
skip_upload: auto   # skip for prereleases; publish stable releases
```

## Full example

```yaml
crates:
  - name: myapp
    publish:
      nix:
        name: myapp
        path: pkgs/myapp/default.nix
        description: "A fast, cross-platform CLI tool"
        homepage: https://github.com/my-org/myapp
        license: mit
        repository:
          owner: my-org
          name: nixpkgs
          branch: main
          token: "{{ Env.GITHUB_TOKEN }}"
        dependencies:
          - name: openssl
          - name: libiconv
            os: darwin
        skip_upload: auto
```
