#!/usr/bin/env bash
# Per-host test runner for the cross-OS gate (`task test:os`). Run from the
# repo root; exits non-zero if any leg of the suite fails.
#
# Prefer `cargo nextest` — the repo's canonical runner (`task test`, `task ci`,
# and CI all use it). `cargo test` runs every test as a thread inside ONE
# process, so a test that writes an executable stub and then has production code
# exec it races every sibling thread's `fork`: the fork briefly inherits the
# stub's writable fd and the exec fails with ETXTBSY until that child reaches
# its own (CLOEXEC) exec. Under heavy fork load that surfaces as a random
# handful of fake-tool tests failing (e.g. `preflight::...::publish_simulation_
# spawn::*`). nextest gives each test its own process, so no sibling fork can
# hold another test's fd — the whole class disappears. nextest does not run
# doctests, so a second `cargo test --doc` pass preserves that coverage.
#
# Hosts without nextest fall back to plain `cargo test` (full suite incl.
# doctests). That leg keeps the shared-process flake risk until nextest is
# provisioned there; the gate stays correct either way (it can produce a false
# RED under extreme load, never a false GREEN).
set -uo pipefail

if cargo nextest --version >/dev/null 2>&1; then
  echo "[suite] cargo nextest run --workspace + cargo test --doc"
  cargo nextest run --workspace --no-fail-fast || exit 1
  cargo test --workspace --doc || exit 1
else
  echo "[suite] cargo nextest not found — falling back to cargo test --workspace"
  cargo test --workspace --no-fail-fast || exit 1
fi
