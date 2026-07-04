#!/usr/bin/env bash
# Per-host test runner for the cross-OS gate (`task test:os`). Run from the
# repo root; exits non-zero if any leg of the suite fails.
#
# The canonical workspace-suite definition lives in exactly ONE place: the
# Taskfile `test` target (nextest + a `cargo test --doc` pass for the doctests
# nextest skips). This script runs THAT target when `task` is on PATH so a
# remote OS leg can never prove a different suite than `task test`/`task ci`/CI.
# Remote validation hosts ship this script inside the git bundle and may lack
# `go-task`, so a fallback reproduces the same two passes directly.
#
# Why nextest (here and in the Taskfile): `cargo test` runs every test as a
# thread inside ONE process, so a test that writes an executable stub and then
# has production code exec it races every sibling thread's `fork` — the fork
# briefly inherits the stub's writable fd and the exec fails with ETXTBSY until
# that child reaches its own (CLOEXEC) exec. Under heavy fork load that surfaces
# as a random handful of fake-tool tests failing. nextest gives each test its
# own process, so no sibling fork can hold another test's fd.
set -uo pipefail

if command -v task >/dev/null 2>&1; then
  echo "[suite] task test (nextest run --workspace + cargo test --doc)"
  task test || exit 1
elif cargo nextest --version >/dev/null 2>&1; then
  # No go-task on this host: reproduce `task test`'s two passes directly. These
  # two lines must stay verbatim-equal to the Taskfile `test` target's cmds;
  # audit-workflow-lockstep fails CI if a Taskfile pass is missing here.
  echo "[suite] cargo nextest run --workspace + cargo test --doc"
  cargo nextest run --workspace --no-fail-fast || exit 1
  cargo test --workspace --doc || exit 1
else
  # No nextest either: plain `cargo test` runs the full suite incl. doctests,
  # keeping the shared-process flake risk until nextest is provisioned. The
  # gate stays correct — it can produce a false RED under extreme load, never a
  # false GREEN.
  echo "[suite] cargo nextest not found — falling back to cargo test --workspace"
  cargo test --workspace --no-fail-fast || exit 1
fi
