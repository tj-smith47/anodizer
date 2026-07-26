#!/usr/bin/env bash
# Cross-OS test gate. Proves the EXACT current commit (HEAD) by running the
# workspace test suite on this box plus any configured OS validation hosts. An
# unconfigured or unreachable host is SKIPPED loudly; a host that passes the
# reachability probe but then fails to launch, or whose suite fails, is a hard
# FAIL — never a silent skip. The gate fails if any non-skipped leg fails.
#
# The remote legs are OPT-IN and name no machine in-tree: set the ssh target for
# whichever you have. With neither set the gate still runs the local suite, so a
# fresh clone is never blocked on someone else's hardware.
#
#   TEST_OS_WINDOWS_HOST   ssh target running Windows + PowerShell (unset = skip)
#   TEST_OS_MACOS_HOST     ssh target running macOS               (unset = skip)
#   TEST_OS_WINDOWS_USER   principal for the Windows scheduled task (default: administrator)
#   TEST_OS_POLL_SECS      poll interval, seconds (default 30)
#   TEST_OS_TIMEOUT_SECS   per-run deadline, seconds (default 3600)
#   TEST_OS_SKIP_LOCAL=1   skip the local leg
#
# HEAD is shipped to each host as a git bundle (not fetched from origin) because
# a pre-push gate must verify the commit about to ship — which is not on origin
# yet — and the validation hosts hold no registry credentials. Each remote
# hard-verifies it landed on $SHA before testing, so a bad checkout can never
# masquerade as a green run of the wrong tree.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

# Machine names are configuration, not code: a clone must not carry anyone's
# hostnames. `.claude/test-os.env` is gitignored, so a maintainer keeps their
# ssh targets there once instead of exporting them into every shell, and a
# contributor who has no such file simply gets the local leg.
if [ -f .claude/test-os.env ]; then
  # shellcheck source=/dev/null
  . .claude/test-os.env
fi

SHA="$(git rev-parse HEAD)"
SHORT="$(git rev-parse --short HEAD)"
WINDOWS_HOST="${TEST_OS_WINDOWS_HOST:-}"
MACOS_HOST="${TEST_OS_MACOS_HOST:-}"
WINDOWS_USER="${TEST_OS_WINDOWS_USER:-administrator}"
CACHE="$HOME/.cache/anodizer-gate"
BUNDLE="$CACHE/anodizer-$SHORT.bundle"
POLL="${TEST_OS_POLL_SECS:-30}"
[ "$POLL" -ge 1 ] 2>/dev/null || POLL=1 # never busy-spin on POLL=0
DEADLINE="${TEST_OS_TIMEOUT_SECS:-3600}"
[ "$DEADLINE" -ge 1 ] 2>/dev/null || DEADLINE=3600 # non-numeric must not zero out the poll loop

say() { printf '\033[1m[test:os]\033[0m %s\n' "$*"; }
# `exit 0` (not `true`) so the probe works on a PowerShell login shell too — a
# Windows host has no `true`, which would mark it "offline" on every run. The
# generous connect timeout tolerates these hosts' slow handshake (and a Mac just
# waking). RETRY before declaring a host unreachable: a single slow/dropped
# handshake on a genuinely-online host must not silently SKIP it out of the gate
# — that one-shot false negative is the exact failure-hiding the gate exists to
# prevent (an online Mac was dropped to SKIP mid-run by a transient hiccup). Only
# a host that fails every attempt is truly offline → SKIP.
reachable() {
  local i
  for i in 1 2 3; do
    timeout 30 ssh -o ConnectTimeout=20 -o BatchMode=yes "$1" "exit 0" >/dev/null 2>&1 && return 0
    [ "$i" -lt 3 ] && sleep 3
  done
  return 1
}
# Read an rc preserving a leading '-' (a negative Windows rc must read back
# faithfully); empty when the rc file is absent (host still running).
rc_of() { grep -oE -- '-?[0-9]+' | head -1; }
declare -A R
declare -A HOST_OF

# Ship the commit under test (HEAD, not `master` — they differ off-master) as a
# bundle, then launch each reachable host non-blocking so its slow Windows/macOS
# build overlaps the local run below.
mkdir -p "$CACHE"
find "$CACHE" -name 'anodizer-*.bundle' -mtime +7 -delete 2>/dev/null # prune stale bundles
git bundle create "$BUNDLE" HEAD >/dev/null || { say "bundle create failed for $SHORT"; exit 1; }
say "bundled $SHORT ($(du -h "$BUNDLE" | cut -f1))"

launch_windows() {
  local host="$1"
  scp -q "$BUNDLE" "$host:gate.bundle" || return 1
  local b64
  b64="$(base64 -w0 .claude/scripts/test-os-windows.ps1)" || return 1
  # Stop any still-running prior task first so two cargo runs can't clobber the
  # shared log/rc. Then: move the bundle to a fixed absolute path the S4U runner
  # reads regardless of principal home dir (decoupled from $USERPROFILE); CLONE
  # the repo if the box is fresh/wiped so the Windows leg can never sit
  # permanently red awaiting a manual seed; install this commit's runner; clear
  # any stale rc; (re)start the detached S4U task (survives ssh logout / the
  # ~39-min Windows session cap that reaps channel-held child process trees).
  # Launch stderr is captured so a FAIL(launch) is diagnosable.
  # The runner is written UTF-8 WITH a BOM (the 3-arg WriteAllText overload):
  # Windows PowerShell 5.1 reads a BOM-less file as ANSI, so any non-ASCII byte
  # in the script (e.g. an em-dash) corrupts a string literal and the whole file
  # fails to parse — powershell exits 1 before running a line, leaving no rc and
  # a stale log that the gate can only resolve as a one-hour timeout.
  # SC2029: $b64/$SHA/$WINDOWS_USER are MEANT to expand client-side — we inject
  # this commit's bundled runner + SHA into the remote command.
  # shellcheck disable=SC2029
  ssh "$host" "Stop-ScheduledTask -TaskName 'anodizer-test-gate' -ea 0; Copy-Item (Join-Path \$env:USERPROFILE 'gate.bundle') 'C:\gate.bundle' -Force; if (-not (Test-Path 'C:\anodizer\.git')) { git clone -q 'C:\gate.bundle' 'C:\anodizer' }; [IO.File]::WriteAllText('C:\anodizer\test-os-windows.ps1',[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('$b64')),[Text.Encoding]::UTF8); Remove-Item C:\anodizer\win_test_gate.rc -ea 0; \$a=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -File C:\anodizer\test-os-windows.ps1 -Sha $SHA'; \$p=New-ScheduledTaskPrincipal -UserId '$WINDOWS_USER' -LogonType S4U -RunLevel Highest; Register-ScheduledTask -TaskName 'anodizer-test-gate' -Action \$a -Principal \$p -Force | Out-Null; Start-ScheduledTask -TaskName 'anodizer-test-gate'" >"$CACHE/launch-windows.err" 2>&1
}
poll_windows() { ssh "$1" "Get-Content C:\anodizer\win_test_gate.rc -ea 0" 2>/dev/null | rc_of; }

launch_macos() {
  local host="$1"
  scp -q "$BUNDLE" "$host:gate.bundle" || return 1
  # Run clone/fetch/checkout SYNCHRONOUSLY (set -e) so a setup failure surfaces
  # as a non-zero ssh exit (→ FAIL(launch)) instead of a one-hour timeout;
  # hard-verify HEAD==$SHA; background ONLY the suite. The runner script ships
  # inside the bundle (it lives in-repo), so the checkout makes it available at
  # `.claude/scripts/test-os-suite.sh` — no separate copy. caffeinate holds the
  # Mac awake; nohup detaches the build from this ssh.
  # SC2029: $SHA is MEANT to expand client-side (inject this commit into checkout).
  # shellcheck disable=SC2029
  ssh "$host" "set -e; cd ~; [ -d anodizer/.git ] || git clone -q gate.bundle anodizer; cd anodizer; git fetch -q ~/gate.bundle HEAD; git checkout -q -f $SHA; [ \"\$(git rev-parse HEAD)\" = $SHA ]; rm -f mac_test_gate.rc; nohup caffeinate -is bash -lc 'cd ~/anodizer && CARGO_BUILD_JOBS=4 bash .claude/scripts/test-os-suite.sh > mac_test_gate.log 2>&1; echo \$? > mac_test_gate.rc' >/dev/null 2>&1 &" >"$CACHE/launch-macos.err" 2>&1
}
poll_macos() { ssh "$1" "cat ~/anodizer/mac_test_gate.rc 2>/dev/null" 2>/dev/null | rc_of; }

# Roles are fixed; the machine behind each is configuration. A role with no host
# configured is reported, not silently dropped — "I never ran Windows" and "I ran
# Windows and it passed" must never look alike in the verdict.
ROLES="windows macos"
HOST_OF[windows]="$WINDOWS_HOST"
HOST_OF[macos]="$MACOS_HOST"

# A role whose host passes reachable() MUST end up either in ACTIVE or marked
# FAIL — never left unset to default-SKIP (that would hide a launch failure).
ACTIVE=""
for role in $ROLES; do
  host="${HOST_OF[$role]}"
  if [ -z "$host" ]; then
    say "SKIP $role (no host configured)"; R[$role]="SKIP(not-configured)"; continue
  fi
  if ! reachable "$host"; then
    say "SKIP $role (unreachable)"; R[$role]="SKIP(unreachable)"; continue
  fi
  if "launch_$role" "$host"; then
    ACTIVE="$ACTIVE $role"; say "launched $role"
  else
    R[$role]="FAIL(launch)"; say "$role FAILED to launch (see $CACHE/launch-$role.err)"
  fi
done

# Local leg runs while the remote builds churn. Delegates to the shared
# per-host runner (`test-os-suite.sh`): nextest where present (process-per-test
# — avoids the `cargo test` shared-process ETXTBSY flake) plus a `cargo test
# --doc` pass, else plain `cargo test`. Same script the remotes run, so every
# leg applies an identical runner policy.
if [ "${TEST_OS_SKIP_LOCAL:-0}" = 1 ]; then
  R[local]="SKIP(opt-out)"
else
  say "local: test-os-suite.sh"
  if bash .claude/scripts/test-os-suite.sh; then R[local]=PASS; else R[local]=FAIL; fi
fi

# Poll each still-running remote role to completion.
waited=0
while [ -n "$(echo "$ACTIVE" | xargs)" ] && [ "$waited" -lt "$DEADLINE" ]; do
  still=""
  for role in $ACTIVE; do
    rc="$("poll_$role" "${HOST_OF[$role]}")"
    if [ -n "$rc" ]; then
      [ "$rc" = 0 ] && R[$role]=PASS || R[$role]="FAIL(rc=$rc)"
      say "$role finished rc=$rc"
    else
      still="$still $role"
    fi
  done
  ACTIVE="$(echo "$still" | xargs)"
  [ -n "$ACTIVE" ] && { sleep "$POLL"; waited=$((waited + POLL)); }
done
for role in $ACTIVE; do R[$role]="FAIL(timeout)"; done

echo
say "verdict ($SHORT):"
fail=0
ran=0
for role in local $ROLES; do
  # Default to FAIL, not SKIP: an unset result for a role we processed is an
  # internal bug, and the gate must surface it rather than pass silently.
  v="${R[$role]:-FAIL(internal-unset)}"
  printf '  %-10s %s\n' "$role" "$v"
  case "$v" in
    FAIL*) fail=1 ;;
    PASS) ran=$((ran + 1)) ;;
  esac
done
# A run where every leg SKIPped (e.g. TEST_OS_SKIP_LOCAL=1 with no hosts
# configured) executed ZERO suites — "GATE PASS" there is the empty-coverage
# illusion the gate exists to kill. Demand at least one real PASS.
if [ "$ran" = 0 ]; then say "GATE INCONCLUSIVE (no suite ran — every leg skipped)"; exit 1; fi
if [ "$fail" = 0 ]; then say "GATE PASS ($ran leg(s) green)"; else say "GATE FAILED"; exit 1; fi
