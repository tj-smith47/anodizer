+++
title = "GitHub Action"
description = "Inputs and outputs exposed by the anodizer-action GitHub Action."
weight = 60
template = "section.html"
+++

# GitHub Action

The action lives at [tj-smith47/anodizer-action](https://github.com/tj-smith47/anodizer-action).
Each row maps to a single Action input.

| Input | Status | Notes |
|---|---|---|
| `from-source` | ✅ Verified | Used by both anodizer's and cfgd's release workflows |
| `install-rust` | ✅ Verified | Used by both anodizer's and cfgd's release workflows |
| `args` | ✅ Verified | Used by both anodizer's and cfgd's release workflows |
| `from-artifact` | ✅ Verified | anodizer reuses build artifacts across jobs |
| `artifact-run-id` | ✅ Verified | anodizer reuses build artifacts across jobs |
| `artifact-workflow` | ✅ Verified | anodizer reuses build artifacts across jobs |
| `install` | ✅ Verified | All eight on-demand tools install: zig, cargo-zigbuild, upx, nfpm, makeself, snapcraft, rpmbuild, cosign |
| `gpg-private-key` | ✅ Verified | Used in cfgd's release |
| `docker-registry` | ✅ Verified | Used in cfgd's release |
| `docker-password` | ✅ Verified | Used in cfgd's release |
| `upload-dist` | ✅ Verified | cfgd's split to merge flow |
| `download-dist` | ✅ Verified | cfgd's split to merge flow |
| `resolve-workspace` | ✅ Verified | cfgd's workspace fan-out |
