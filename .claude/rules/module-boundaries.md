# anodizer module boundaries

This rule documents which modules are allowed to call `std::process::Command::new`
(or any other subprocess-spawning API) directly. The aim: keep subprocess
invocations confined to a small, auditable allow-list so security review can
focus on the few touch-points that actually shell out.

## Allow-list (may call `Command::new` directly)

- `crates/core/src/git/**` — git porcelain (`clone`, `tag`, `push`, ...).
  Carved from former `git.rs` into focused submodules (`semver`, `detect`,
  `status`, `remote`, `tags`, `commits`, `github_api`); all sibling files
  inherit the allow-listing.
- `crates/core/src/hooks.rs` — user-defined `before:` / `after:` hook execution.
- `crates/core/src/cargo_lock.rs` — `cargo update --workspace` invoked by the
  `tag` command after a version bump.
- `crates/core/src/docker_detect.rs` — `docker buildx version` probe used by
  the `check` command.
- `crates/core/src/tool_detect.rs` — generic `<tool> --version` probe used by
  the `healthcheck` command.
- `crates/core/src/partial.rs` — `rustc -vV` probe for host-target detection
  (consumed by `partial.by` resolution).
- `crates/core/src/user_command.rs` — sandboxed `Command` constructor for
  user-supplied commands (`publisher.cmd`); env is whitelisted to prevent
  credential leakage.
- `crates/stage-*/**` — every file under any stage crate is allow-listed.
  The bullets below are *representative* (the primary tool each stage wraps),
  not exhaustive — utility submodules like `stage-publish/util.rs` (which
  spawns `git`/`gh` on behalf of the per-publisher modules) are also covered
  by the umbrella glob. The named bullets call out the load-bearing tool per
  stage so a reviewer reading this rule knows what to expect.
  - `stage-build` (cargo, rustup, cross)
  - `stage-archive` (tar, zip, sbom inputs)
  - `stage-docker` (docker, buildx, podman)
  - `stage-sign` (cosign, gpg)
  - `stage-notarize` (codesign, xcrun, notarytool, stapler)
  - `stage-msi` (wix, candle, light)
  - `stage-nsis` (makensis)
  - `stage-pkg` (pkgbuild, productbuild)
  - `stage-dmg` (hdiutil, mkisofs, genisoimage)
  - `stage-snapcraft` (snapcraft)
  - `stage-source` (git archive)
  - `stage-makeself` (makeself)
  - `stage-publish/aur*` (git over ssh for AUR)
  - `stage-publish/cargo` (cargo publish, cargo package, cargo logout)
  - `stage-publish/nix` (nix-instantiate, nix-build, git for the nix repo)
  - `stage-changelog` (git log)
  - `stage-upx` (upx)
  - `stage-srpm` (rpmbuild)
  - `stage-universal` (lipo)
  - `stage-blob/kms` (gcloud / az / aws CLI for KMS)
  - `stage-checksum` (cosign for blob signing only when configured)

## Forbid-list (must NOT call `Command::new` directly)

- `crates/cli/**` — orchestration only; delegate to a stage or `core::git`
  (or one of the other allow-listed `core::<tool>_*` helpers).
- `crates/core/**` (apart from the allow-listed modules above) — keep core
  pure / library-grade. If you need a new shell-out, extract a helper module
  next to `git/` and add it to the allow-list above.
- Any new crate that doesn't appear in the allow-list above.

## Rationale

Each subprocess invocation is an authorization boundary: it can write to
disk, call the network, exfiltrate secrets via `argv` or `env`, and is
opaque to clippy / unsafe / panic-safety review. Confining `Command::new`
to a handful of named modules means the security-relevant surface is small
and reviewable.

The current count (audit 2026-05-06, post-git-carve): **50 files
/ 198 call sites**, all inside the allow-list above. The file-count
delta vs. the prior 35-file baseline is structural — `git.rs` was
carved into a `git/` module of 7 sibling submodules + tests — not
new shell-out surface. Drift to a forbidden module is a review-blocker.

## Enforcement

- Code review (manual until the post-edit hook is extended).
- Optional future: extend `.claude/hooks/post-edit.sh` to grep
  `Command::new` and reject in any non-allow-listed file path.
- Mirror cfgd's `module-boundaries.md` rule pattern.
