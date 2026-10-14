//! The body of a Homebrew `livecheck do … end` block, for a formula and for
//! a cask.
use anodizer_core::template::ruby_escape_str;
use anyhow::Result;

/// Render the body of a formula's `livecheck do … end` block from config.
/// An active block with no `url:` reads `url :stable`.
///
/// The returned string is the inner lines, with no `do`/`end` and no leading
/// indent (the template indents each line):
///
/// - config absent, `skip: true`, or none of `strategy`/`url`/`regex` set ⇒
///   `skip "<reason>"` (default reason `Auto-generated on release.`). A binary
///   tap's archive URL/sha change every release, so an active livecheck has
///   nothing stable to poll.
/// - `skip` unset or `false` with a `strategy`/`url`/`regex` ⇒ an active block
///   (`url :stable` / `strategy :github_latest` / `regex(...)`).
///
/// `skip: false` with none of the three also logs a warning, because an empty
/// `livecheck do … end` is invalid Ruby and the block falls back to `skip`.
/// A `url:` naming `url`, the cask-only symbol, is an error.
pub fn render_livecheck(
    cfg: Option<&anodizer_core::config::HomebrewLivecheck>,
    log: &anodizer_core::log::StageLogger,
) -> Result<Option<String>> {
    render_livecheck_for(cfg, &FORMULA_LIVECHECK_URLS, log)
}

/// [`render_livecheck`] for a cask. An active block with no `url:` reads
/// `url :url`, the cask's own download URL. A `url:` naming `stable` or
/// `head` is an error, because Homebrew refuses those symbols in a cask
/// ("does not reference a checkable URL"); only a formula has those URLs.
pub fn render_cask_livecheck(
    cfg: Option<&anodizer_core::config::HomebrewLivecheck>,
    log: &anodizer_core::log::StageLogger,
) -> Result<Option<String>> {
    render_livecheck_for(cfg, &CASK_LIVECHECK_URLS, log)
}

/// The livecheck `url` symbols Homebrew resolves for one kind of package,
/// the first being the default when `url:` is unset.
struct LivecheckUrls {
    owner: &'static str,
    symbols: &'static [&'static str],
}

const FORMULA_LIVECHECK_URLS: LivecheckUrls = LivecheckUrls {
    owner: "formula",
    symbols: &["stable", "head", "homepage"],
};

const CASK_LIVECHECK_URLS: LivecheckUrls = LivecheckUrls {
    owner: "cask",
    symbols: &["url", "homepage"],
};

fn render_livecheck_for(
    cfg: Option<&anodizer_core::config::HomebrewLivecheck>,
    urls: &LivecheckUrls,
    log: &anodizer_core::log::StageLogger,
) -> Result<Option<String>> {
    let cfg = cfg.cloned().unwrap_or_default();
    let has_active = cfg.strategy.as_deref().is_some_and(|s| !s.is_empty())
        || cfg.url.as_deref().is_some_and(|s| !s.is_empty())
        || cfg.regex.as_deref().is_some_and(|s| !s.is_empty());
    let skip = match cfg.skip {
        Some(s) => s,
        None => !has_active,
    };
    if skip || !has_active {
        if cfg.skip == Some(false) && !has_active {
            log.warn(
                "homebrew livecheck: `skip: false` was set but no \
                 `strategy`/`url`/`regex` was provided — an active livecheck \
                 needs at least one. Falling back to `skip`.",
            );
        }
        let reason = cfg
            .skip_reason
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("Auto-generated on release.");
        return Ok(Some(format!("skip \"{}\"", ruby_escape_str(reason))));
    }

    let mut lines: Vec<String> = Vec::new();
    let url_val = cfg
        .url
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(urls.symbols[0]);
    if let Some(sym) = livecheck_url_symbol(url_val) {
        if !urls.symbols.contains(&sym) {
            anyhow::bail!(
                "homebrew livecheck: `url: {url_val}` names a URL a {} does not have; \
                 a {} livecheck takes {} or a literal URL",
                urls.owner,
                urls.owner,
                urls.symbols
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        lines.push(format!("url :{}", sym));
    } else {
        lines.push(format!("url \"{}\"", ruby_escape_str(url_val)));
    }
    if let Some(strategy) = cfg.strategy.as_deref().filter(|s| !s.is_empty()) {
        lines.push(format!("strategy :{}", strategy.trim_start_matches(':')));
    }
    if let Some(regex) = cfg.regex.as_deref().filter(|s| !s.is_empty()) {
        // `regex` is raw Ruby (a `%r{...}` or `/.../` literal), emitted
        // verbatim.
        lines.push(format!("regex({})", regex));
    }
    Ok(Some(lines.join("\n")))
}

/// Map a livecheck `url` config value to its Ruby symbol when it is one of
/// the symbols Homebrew knows (`stable`, `head`, `homepage`, `url`), with or
/// without a leading `:`. Returns `None` for a literal URL, which the caller
/// renders as a quoted string.
fn livecheck_url_symbol(url: &str) -> Option<&str> {
    let u = url.trim().trim_start_matches(':');
    matches!(u, "stable" | "head" | "homepage" | "url").then_some(u)
}

#[cfg(test)]
mod tests {
    use anodizer_core::config::HomebrewLivecheck;
    use anodizer_core::log::{StageLogger, Verbosity};

    fn github_latest() -> HomebrewLivecheck {
        HomebrewLivecheck {
            strategy: Some("github_latest".to_string()),
            ..Default::default()
        }
    }

    /// Homebrew resolves `url :stable` and `url :head` for a formula only and
    /// raises "does not reference a checkable URL" for a cask, so each owner
    /// gets its own default when `url:` is unset.
    #[test]
    fn an_active_livecheck_with_no_url_reads_the_url_its_owner_has() {
        let log = StageLogger::new("publish", Verbosity::Normal);
        assert_eq!(
            super::render_livecheck(Some(&github_latest()), &log)
                .unwrap()
                .as_deref(),
            Some("url :stable\nstrategy :github_latest"),
        );
        assert_eq!(
            super::render_cask_livecheck(Some(&github_latest()), &log)
                .unwrap()
                .as_deref(),
            Some("url :url\nstrategy :github_latest"),
        );
    }

    /// Each owner refuses the symbols only the other one has, since Homebrew
    /// fails the whole cask or formula on a `url` symbol it cannot resolve.
    #[test]
    fn a_livecheck_url_symbol_the_owner_does_not_have_is_an_error() {
        let log = StageLogger::new("publish", Verbosity::Normal);
        let with_url = |url: &str| HomebrewLivecheck {
            url: Some(url.to_string()),
            ..github_latest()
        };
        for url in ["stable", ":head"] {
            let err = super::render_cask_livecheck(Some(&with_url(url)), &log)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("a cask does not have") && err.contains("`url`, `homepage`"),
                "{err}"
            );
        }
        let err = super::render_livecheck(Some(&with_url("url")), &log)
            .unwrap_err()
            .to_string();
        assert!(err.contains("a formula does not have"), "{err}");
        for (url, line) in [("homepage", "url :homepage"), (":url", "url :url")] {
            assert!(
                super::render_cask_livecheck(Some(&with_url(url)), &log)
                    .unwrap()
                    .unwrap()
                    .starts_with(line)
            );
        }
        assert!(
            super::render_livecheck(Some(&with_url("homepage")), &log)
                .unwrap()
                .unwrap()
                .starts_with("url :homepage")
        );
    }

    /// A cask config handed to the formula renderer would render `url :stable`,
    /// which Homebrew refuses in a cask.
    #[test]
    fn no_cask_livecheck_is_rendered_by_the_formula_renderer() {
        use anodizer_core::test_helpers::test_sources::{production_half, rust_sources};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/homebrew");
        let mut cask_calls = 0;
        let mut wrong = Vec::new();
        for path in rust_sources(&dir) {
            let text = std::fs::read_to_string(&path).unwrap();
            for line in production_half(&text).lines() {
                let renders_cask = line.contains("render_cask_livecheck(");
                let renders_formula =
                    line.contains("render_livecheck(") && !line.contains("fn render_livecheck(");
                if renders_cask && !line.contains("fn render_cask_livecheck(") {
                    cask_calls += 1;
                }
                if renders_formula && line.contains("cask") {
                    wrong.push(format!("{}: {}", path.display(), line.trim()));
                }
            }
        }
        assert!(
            wrong.is_empty(),
            "a cask livecheck goes through render_cask_livecheck:\n{}",
            wrong.join("\n")
        );
        assert_eq!(
            cask_calls, 2,
            "the cask renderers: publish_top.rs and cask_scope.rs"
        );
    }
}
