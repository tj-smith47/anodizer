param([Parameter(Mandatory = $true)][string]$Sha)

# Windows leg of `task test:os`. Fetches the exact commit under test from the
# bundle the orchestrator ships (a pre-push gate must prove the not-yet-pushed
# tree), HARD-VERIFIES it landed on $Sha before testing — a failed fetch/checkout
# must never fall through to testing a stale tree and reporting that rc as the
# result — pins the installer-tool PATH the determinism harness uses, runs the
# full workspace suite, and records a tail-able log plus a final RC file the
# orchestrator polls. Invoked by an S4U scheduled task so the build outlives the
# dispatching ssh session (Windows reaps channel-held child trees at the
# ~39-minute session cap).

$ErrorActionPreference = 'Continue'
Set-Location C:\anodizer

$log = 'C:\anodizer\win_test_gate.log'
$rcf = 'C:\anodizer\win_test_gate.rc'
Remove-Item $rcf -ea 0

# Fixed absolute path (NOT $env:USERPROFILE): this runner executes under the
# S4U `administrator` principal whose home may differ from the ssh login user
# that scp'd the bundle, so a home-relative path would silently miss the bundle
# and test a stale tree. The orchestrator's launch ssh places it here and clones
# the repo if the box is fresh, so a checkout can always reach $Sha.
$bundle = 'C:\gate.bundle'
if (Test-Path $bundle) { git fetch -q "$bundle" 'HEAD' 2>&1 | Out-Null }
git checkout -q -f $Sha 2>&1 | Out-Null
$head = (git rev-parse HEAD 2>$null | Out-String).Trim()
if ($head -ne $Sha) {
  "=== ABORT: checkout mismatch head=$head want=$Sha $(Get-Date -Format o) ===" | Out-File $log -Encoding ascii
  # rc 200 is a sentinel distinct from any cargo exit: checkout/commit mismatch.
  '200' | Out-File $rcf -Encoding ascii
  exit 1
}

$machine = [Environment]::GetEnvironmentVariable('PATH', 'Machine')
$user = [Environment]::GetEnvironmentVariable('PATH', 'User')
$env:PATH = "C:\upx-pinned;C:\Program Files\GnuPG\bin;C:\Program Files (x86)\WiX Toolset v3.14\bin;C:\Program Files (x86)\NSIS;C:\cosign-pinned;C:\Program Files\Git\cmd;C:\Program Files\Git\usr\bin;$machine;$user"
$env:CARGO_BUILD_JOBS = '4'

"=== WIN TEST GATE sha=$head $(Get-Date -Format o) ===" | Out-File $log -Encoding ascii
cargo test --workspace --no-fail-fast 2>&1 | Out-File $log -Append -Encoding ascii
$code = $LASTEXITCODE
"=== DONE rc=$code $(Get-Date -Format o) ===" | Out-File $log -Append -Encoding ascii
"$code" | Out-File $rcf -Encoding ascii
