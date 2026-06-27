#!/usr/bin/env bash
# Guard: repository identity (owner/repo/sha) and the GitHub token resolve
# through ONE canonical path each — never re-derived ad hoc.
#
# Two divergence classes this audit makes non-representable:
#
#   A. Repo slug (owner/repo). The single source of truth is
#      `crates/core/src/git/slug.rs` (`RepoSlug` + `resolve_*_slug[_in]`),
#      which applies `config override -> origin remote` once. The remote-URL
#      detectors (`detect_github_repo[_in]`, `detect_owner_repo[_in]`,
#      `parse_github_remote`, `parse_remote_owner_repo`) are crate-private to
#      `core::git` precisely so no other site can re-parse the remote. A call
#      to any of them OUTSIDE `crates/core/src/git/` (where it would not even
#      compile, given the `pub(crate)` visibility — this is the backstop that
#      also catches a new in-`core` site bypassing the resolver) is a
#      violation. Consume `RepoSlug` instead.
#
#   B. GitHub token. The single source of truth is
#      `resolve_github_token_with_env` (+ the `resolve_github_token` /
#      `resolve_rollback_token` wrappers) in
#      `crates/core/src/git/github_api.rs`. It empty-filters every link so a
#      `GITHUB_TOKEN=""` (the shape GitHub Actions materializes for a missing
#      secret) falls through. A hand-rolled `.var("GITHUB_TOKEN")` /
#      `.env_var("ANODIZER_GITHUB_TOKEN")` read OUTSIDE that file re-introduces
#      the missing-filter bug. Route through the canonical resolver instead.
#
# This audit is INTENTIONALLY UNWIRED from Taskfile/CI — the primary
# enforcement is type-level (private fields + crate-private detectors). It
# exists as a grep backstop; wire it into `task lint` in a later batch.
#
# Comment lines and `crates/*/tests/**` are exempt. A genuinely legitimate
# site tags the line with `// slug-ok: <why>` (class A) or
# `// token-ok: <why>` (class B).
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

fail=0

# --- Class A: remote-URL owner parsing outside the slug resolver ------------
slug_hits="$(
    grep -rnE '\b(detect_github_repo|detect_owner_repo|parse_github_remote|parse_remote_owner_repo)\s*\(' \
        crates --include='*.rs' 2>/dev/null \
        | grep -v -E '^[^:]+:[0-9]+:[[:space:]]*//' \
        | grep -v -E '^crates/core/src/git/' \
        | grep -v -E ':[0-9]+:.*//[[:space:]]*slug-ok:' \
        | grep -v -E '/tests/' \
        || true
)"
if [[ -n "$slug_hits" ]]; then
    fail=1
    echo "REPO-IDENTITY: owner/repo re-derived outside the canonical resolver."
    echo
    echo "$slug_hits"
    echo
    echo "These sites parse the git remote directly instead of consuming a"
    echo "RepoSlug from anodizer_core::git::resolve_github_slug[_in] /"
    echo "resolve_repo_slug[_in] (config override -> remote, applied once)."
    echo "Replace the detector call with a resolver call. A truly legitimate"
    echo "site tags the line with  // slug-ok: <why>."
    echo
fi

# --- Class B: hand-rolled GitHub token env reads outside the resolver -------
token_hits="$(
    grep -rnE '\.(var|env_var)\(\s*"(ANODIZER_GITHUB_TOKEN|GITHUB_TOKEN)"' \
        crates --include='*.rs' 2>/dev/null \
        | grep -v -E '^[^:]+:[0-9]+:[[:space:]]*//' \
        | grep -v -E '^crates/core/src/git/github_api.rs:' \
        | grep -v -E ':[0-9]+:.*//[[:space:]]*token-ok:' \
        | grep -v -E '/tests/' \
        || true
)"
if [[ -n "$token_hits" ]]; then
    fail=1
    echo "REPO-IDENTITY: GitHub token resolved without the empty-string filter."
    echo
    echo "$token_hits"
    echo
    echo "These sites read ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN directly,"
    echo "bypassing anodizer_core::git::resolve_github_token_with_env (which"
    echo "empty-filters every link so a GITHUB_TOKEN=\"\" falls through). Route"
    echo "through resolve_github_token_with_env / resolve_github_token /"
    echo "util::resolve_rollback_token. A legitimate site tags  // token-ok: <why>."
    echo
fi

if [[ "$fail" -ne 0 ]]; then
    exit 1
fi

echo "audit-repo-identity: owner/repo + GitHub token both resolve through their canonical path."
