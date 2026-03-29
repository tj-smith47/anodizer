take on the next task (after Platform-Specific Packaging) in .claude/PLAN.md — "Cloud Storage + CI Fan-Out". This includes blob storage upload (S3/GCS/Azure) and split/merge CI fan-out.

## Process

Follow the same process used for the packaging stages session:

1. Read the plan, understand the existing codebase patterns (stage-nfpm, stage-dmg, etc. are good references for new stage crates; config.rs for config structs; pipeline.rs for wiring)
2. Research GoReleaser's implementation of blob storage and split/merge thoroughly before writing code — this is your north star for completeness
3. Implement everything fully. No stubs, no TODOs, no deferred work, no "v0.1 scope" excuses. Every config field must be consumed at runtime, every feature must work end-to-end
4. Don't reinvent things that already exist in the repo — check core/util.rs for shared helpers (apply_mod_timestamp, collect_replace_archives, parse_mod_timestamp, find_binary, etc.), core/target.rs for target helpers, etc. If something should be shared, move it to core
5. Wire into the pipeline, workspace Cargo.toml, and CLI Cargo.toml
6. Write comprehensive tests: config parsing (valid/default/invalid), behavior, dry-run, error paths, command construction
7. After implementation, run a review agent and fix ALL findings of ALL severity levels — not just critical, ALL of them
8. Keep looping review → fix → review until a review round returns ZERO findings of ANY severity level
9. Run `cargo fmt --check && cargo clippy --workspace -- -D warnings && cargo test --workspace` between every review round — all three must be clean

## Key rules

- Fix everything you find, even if it seems "pre-existing" or "out of scope" — the scope is to get this repo ready for release
- Never ask permission to fix known problems — just fix them
- Never label issues as "minor" or "suggestion-only" — all issues get fixed
- Never defer features — the goal is completeness
- Work on master directly, no branches or worktrees
- Do the whole session in one conversation, only split on genuine blockers

## What "done" looks like

- Blob storage stage with S3/GCS/Azure provider support, templated paths, credentials config, dry-run, all GoReleaser-parity fields
- Split/merge CLI flags with JSON serialization, GitHub Actions matrix generation
- All new config fields tested (parsing + behavior + error paths)
- Zero clippy warnings, zero fmt issues, all tests passing
- A final review round that explicitly says "Zero findings"
- PLAN.md updated with actual results
- Clean commit
