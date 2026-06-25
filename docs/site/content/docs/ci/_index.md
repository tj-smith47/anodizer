+++
title = "CI/CD Integration"
description = "Wire anodizer into GitHub Actions (or GitLab CI). Start with the quick-start GitHub Actions job, reach for the full hardened topology when you publish to one-way-door registries, and pick a workspace shape from the strategy decision tree."
sort_by = "weight"
template = "section.html"
+++

Run anodizer from CI to tag, build, sign, and publish a release without hand-rolling the GitHub-Actions plumbing. Read in this order:

1. **[GitHub Actions](@/docs/ci/github-actions.md)** — the quick-start release job and the common patterns (tag-push, auto-tag on push to main, monorepo resolve).
2. **[anodizer-action reference](@/docs/ci/anodizer-action.md)** — every input and output of `tj-smith47/anodizer-action`.
3. **[The Release Pipeline (topology)](@/docs/ci/release-pipeline.md)** — the hardened end-to-end shape: preflight → auto-tag → 4-shard determinism → publish → npm-provenance.
4. **[Release Workflow Strategies](@/docs/ci/release-workflows.md)** — a decision tree and canonical YAML for single-crate, lockstep, per-crate, and split-CI shapes.
5. **[Standalone Pipeline Commands](@/docs/ci/split-merge-ci.md)** — break publish and announce into independent jobs.
6. **[GitLab CI](@/docs/ci/gitlab-ci.md)** — running anodizer outside GitHub Actions.
