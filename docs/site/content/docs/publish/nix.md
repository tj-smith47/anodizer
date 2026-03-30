+++
title = "Nix"
description = "Publish Nix derivations to a nixpkgs-style repository"
weight = 76
template = "docs.html"
+++

The Nix publisher generates a Nix derivation expression for your release and commits it to a nixpkgs-style repository. It is configured under `publish.nix` within a crate's config.

## Minimal config

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
```

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
| `license` | string | `mit` | Nix license identifier (e.g. `mit`, `asl20`, `gpl3Only`). Must be a valid `lib.licenses` attribute. |
| `dependencies` | list | | Runtime dependencies as Nix package names. |
| `formatter` | string | | Nix formatter to run on the generated file: `alejandra` or `nixfmt`. |

### Repository config

```yaml
repository:
  owner: my-org
  name: nixpkgs
  branch: main           # target branch
  token: "{{ .Env.GITHUB_TOKEN }}"
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

The `license` field must be a valid `lib.licenses` attribute from nixpkgs. Common values include `mit`, `asl20`, `gpl3Only`, `gpl3Plus`, `lgpl21Only`, `mpl20`, `isc`, `bsd2`, `bsd3`, `unlicense`, `asl20`.

Run `anodize check` to validate the license identifier before releasing.

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
          token: "{{ .Env.GITHUB_TOKEN }}"
        dependencies:
          - name: openssl
          - name: libiconv
            os: darwin
        skip_upload: auto
```
