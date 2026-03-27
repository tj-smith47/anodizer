# Anodize Documentation Site — Phase B: Parity, Polish & Content Completion

> **For agentic workers:** This is an independent follow-up to the Phase A docsite implementation. Start by doing your own analysis of what's missing — compare the live site against GoReleaser's site structure, check for Coming Soon pages that should now have real content, and identify polish gaps. The known issues below are a starting point, not an exhaustive list.

**Goal:** Bring the anodize documentation site to GoReleaser-level quality — complete content for all implemented features, matching navigation structure, and production polish.

**Depends on:** Phase A complete (docs/site/ with Zola, templates, ~45 content pages, xtask). Also depends on Session 5 extended features being merged to master (most features that were "Coming Soon" are now implemented).

**Spec:** `docs/superpowers/specs/2026-03-27-docsite-design.md`

---

## Before You Start: Independent Analysis

Do NOT just execute the tasks below blindly. The Phase A implementation made some structural deviations from the original plan (directory names, nav grouping) and Session 5 may have added features not anticipated here. You must:

1. **Compare against GoReleaser's site** — fetch https://goreleaser.com and walk their navigation. Compare section-by-section against our sidebar. Identify structural gaps.
2. **Audit Coming Soon pages** — check which features are now implemented in the codebase. For each Coming Soon page, grep the crate source to see if the feature exists. Replace skeletons with real content for implemented features.
3. **Check the xtask output** — run `cargo xtask gen-docs` and verify the CLI reference covers all current commands and flags. Check for empty help text cells.
4. **Browse the built site** — build with `zola build`, serve with `python3 -m http.server`, and click through every page. Note broken layouts, missing content, dead links.
5. **Read the GoReleaser comparison spec** — `.claude/specs/test-coverage-comparison.md` and `.claude/specs/parity-gap-analysis.md` for feature context.

Then create your own task list from what you find, merging with the known issues below.

---

## Known Issues from Phase A Review

### Navigation Parity

GoReleaser's top nav: Getting Started | Documentation | More Resources | Blog | Sponsors | Pro | GitHub

Ours: Docs | Migration | GitHub

At minimum, "Getting Started" should be a separate top-level nav item (not buried inside Docs). Consider adding a "Resources" equivalent.

The sidebar grouping also deviates — our "More" section groups Changelog + CI/CD pages, while GoReleaser has these as distinct sidebar sections. Evaluate whether our grouping is better or worse.

### Coming Soon Pages That May Need Real Content

These were skeleton pages in Phase A. Session 5 implemented many of these features. Check the codebase for each:

| Page | Feature | Where to check |
|------|---------|---------------|
| `builds/universal-binaries.md` | macOS universal binaries (Task 5F) | `crates/stage-build/src/` |
| `builds/upx.md` | UPX compression (Task 5K) | `crates/stage-build/src/` or separate crate |
| `packages/source-sbom.md` | Source archives + SBOM (Task 5J) | look for source/sbom stage or config |
| `publish/chocolatey.md` | Chocolatey publisher (Task 5H) | `crates/stage-publish/src/` |
| `publish/winget.md` | Winget publisher (Task 5H) | `crates/stage-publish/src/` |
| `publish/aur.md` | AUR publisher (Task 5I) | `crates/stage-publish/src/` |
| `publish/krew.md` | Krew publisher (Task 5I) | `crates/stage-publish/src/` |
| `advanced/config-includes.md` | Config includes (Task 5D) | `crates/core/src/config.rs` or pipeline |
| `advanced/reproducible-builds.md` | Reproducible builds (Task 5E) | `crates/stage-build/src/` |

### Possible New Pages Needed

Session 5 may have added features that have no documentation page at all:

| Feature | Task | Check |
|---------|------|-------|
| Additional announce providers (Telegram, Teams, Mattermost, SMTP) | 5L | `crates/stage-announce/src/` |
| JSON Schema command | 5M | `crates/cli/src/commands/` |
| `.env` file loading | 5M | `crates/core/src/` |
| Build ignore list | 5M | config.rs |
| Build per-target overrides | 5M | config.rs |
| cargo-binstall metadata | 5A | Already has page content, verify accuracy |
| Version sync from tags | 5A | Verify docs match implementation |

### Polish Items

- **Search** — Zola has built-in elasticlunr.js search. Enable it in config.toml and add a search input to the nav.
- **Blog** — Section exists but is empty. At minimum, remove it from nav or add a placeholder post.
- **Syntax highlighting** — Now using Dracula theme. Verify it looks good on all code blocks (YAML, Rust, bash, TOML).
- **Mobile experience** — Hamburger menu exists but test it thoroughly. The sidebar toggle uses inline JS — consider if this is sufficient.
- **Internal link audit** — Some content pages have `@/docs/...` links that may reference moved paths (the Phase A implementation reorganized some directories).
- **xtask gen-docs** — The config reference is manually maintained. Verify all fields in `Config` struct are listed. Run `--check` to verify freshness.
- **GoReleaser migration guide** — Now that we know GoReleaser has experimental Rust support, the migration guide should address "why switch from GoReleaser's Rust builder to anodize" with honest comparison of what each provides.

### Content Accuracy

Phase A code reviews found and fixed several factual errors. The next session should do another pass:
- Verify all config field names match the actual `config.rs` struct field names (accounting for `#[serde(rename)]` and `#[serde(alias)]` attributes)
- Verify all config field types and defaults are accurate
- Verify CLI flag documentation matches `lib.rs` definitions
- Check that code examples use valid YAML that would actually parse

---

## Structural Notes

**Directory layout (as built, may differ from original spec):**
- Content lives at `docs/site/content/docs/` with subsections: `getting-started/`, `general/`, `builds/`, `packages/` (plural, not `package/`), `sign/`, `publish/`, `announce/`, `ci/`, `advanced/`, `more/` (contains changelog), `reference/` (contains auto-generated cli.md and configuration.md)
- Migration content at `docs/site/content/migration/`
- Sidebar is a partial at `docs/site/templates/partials/sidebar.html` — any new pages must be added here too
- Auto-generated pages are at `docs/site/content/docs/reference/cli.md` and `configuration.md` — regenerate with `cargo xtask gen-docs`

**To preview the site locally:**
```bash
# If accessing from another machine, temporarily change base_url in config.toml to http://<vm-ip>:8081
cd docs/site && zola build && cd public && python3 -m http.server 8081 --bind 0.0.0.0
```
