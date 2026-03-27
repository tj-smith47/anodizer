+++
title = "GitHub Actions"
description = "Automate releases with GitHub Actions"
weight = 1
template = "docs.html"
+++

## Basic workflow

```yaml
name: Release

on:
  push:
    tags: ["v*"]

permissions:
  contents: write

jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0    # needed for changelog generation

      - uses: dtolnay/rust-toolchain@stable

      - name: Install anodize
        run: cargo install anodize

      - name: Install cross-compilation tools
        run: cargo install cargo-zigbuild

      - name: Release
        run: anodize release
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

## With auto-tagging

Combine with `anodize tag` to automatically create tags from commit messages:

```yaml
name: Release

on:
  push:
    branches: [main]

permissions:
  contents: write

jobs:
  tag:
    runs-on: ubuntu-latest
    outputs:
      new_tag: ${{ steps.tag.outputs.new_tag }}
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo install anodize
      - id: tag
        run: anodize tag
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}

  release:
    needs: tag
    if: needs.tag.outputs.new_tag != ''
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo install anodize cargo-zigbuild
      - run: anodize release
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

## Caching

Speed up CI by caching the cargo install:

```yaml
- uses: actions/cache@v4
  with:
    path: ~/.cargo/bin/anodize
    key: anodize-${{ runner.os }}
```
