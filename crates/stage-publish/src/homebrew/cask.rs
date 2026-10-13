//! Homebrew cask generation: template, params struct, and the per-context
//! cask body builder used by both per-crate and top-level publishers.
use anodizer_core::context::Context;
use anodizer_core::template::ruby_escape_str;
use anyhow::{Context as _, Result};
/// Column of the first argument of a top-level `url "…"` stanza, where its
/// keyword-argument continuation lines start.
pub(super) const URL_ARGS_INDENT: &str = "      ";
/// Column of the first argument of a `url "…"` stanza nested inside
/// `on_<os> do` and `on_<arch> do`.
pub(super) const ARCH_URL_ARGS_INDENT: &str = "          ";

// ---------------------------------------------------------------------------
// generate_cask
// ---------------------------------------------------------------------------

/// Cask `binary` template entry.
///
/// The bare-string YAML form `"my-cli"` deserialises to `name = "my-cli",
/// target = None`, which renders `binary "my-cli"`. The structured form
/// `{ name, target }` preserves the rename so the template emits
/// `binary "my-cli", target: "mycli"`.
#[derive(serde::Serialize, Clone)]
pub struct CaskBinaryEntry {
    /// Path inside the .app bundle.
    pub name: String,
    /// Optional rename target — symlink name in `/usr/local/bin`.
    pub target: Option<String>,
}

/// Multi-platform cask architecture entry (within an OS block).
#[derive(serde::Serialize, Clone)]
pub struct CaskArchEntry {
    pub(super) arch_block: String, // "intel" or "arm"
    pub(super) url: String,
    pub(super) sha256: String,
}

/// Multi-platform cask OS block (on_macos / on_linux).
#[derive(serde::Serialize, Clone)]
pub struct CaskPlatformBlock {
    pub(super) os_block: String, // "macos" or "linux"
    pub(super) arches: Vec<CaskArchEntry>,
}

/// Parameters for cask generation.
pub struct CaskParams<'a> {
    /// Cask token (lowercase hyphenated name used as `cask "name"`).
    pub name: &'a str,
    /// Display name shown in `name` stanza.
    pub display_name: &'a str,
    /// Alternative display names.
    pub alternative_names: &'a [String],
    /// Release version string.
    pub version: &'a str,
    /// SHA-256 hex digest of the download artifact.
    pub sha256: &'a str,
    /// Download URL for the .dmg/.zip/.app archive.
    pub url: &'a str,
    /// Project homepage URL.
    pub homepage: Option<&'a str>,
    /// One-line description.
    pub description: Option<&'a str>,
    /// macOS .app bundle name (e.g. "MyApp.app").
    pub app: Option<&'a str>,
    /// Binary stubs to create in /usr/local/bin (inside the .app).
    /// Each entry carries `{ name, target: Option<String> }` so the
    /// template can emit `binary "<n>"` (bare) or `binary "<n>", target: "<t>"`
    /// (rename form).
    pub binaries: &'a [CaskBinaryEntry],
    /// Post-install user-facing notes, rendered as the body of a
    /// `caveats <<~EOS` heredoc with each line indented under it.
    pub caveats: Option<&'a str>,
    /// Pre-rendered `zap` stanza Ruby (full `zap launchctl: [...], quit: [...]`
    /// block) — empty string when no `zap` config is present.  Built via
    /// `render_zap_block`.
    pub zap_block: &'a str,
    /// Pre-rendered `uninstall` stanza Ruby (full `uninstall launchctl: [...]`
    /// block) — empty string when no `uninstall` config is present.  Built
    /// via `render_uninstall_block`.
    pub uninstall_block: &'a str,
    /// Custom Ruby code, placed at the top of the cask body (where Homebrew
    /// expects arbitrary helper methods) and re-indented one level.
    pub custom_block: Option<&'a str>,
    /// Path inside the download of a macOS Services-menu bundle, rendered as
    /// `service "<path>"`. A cask `service` installs that bundle into
    /// `~/Library/Services`; it is not a launchd daemon.
    pub service: Option<&'a str>,
    /// Pre-rendered `livecheck do … end` block body (the lines *between*
    /// `livecheck do` and `end`, already trimmed — the template indents each).
    /// `None` ⇒ the default `skip "Auto-generated on release."` line, matching
    /// today's cask behaviour. Build via
    /// [`render_livecheck`](super::formula::render_livecheck).
    pub livecheck: Option<String>,
    /// Manpage file paths.
    pub manpages: &'a [String],
    /// Bash completion file path.
    pub completions_bash: Option<&'a str>,
    /// Zsh completion file path.
    pub completions_zsh: Option<&'a str>,
    /// Fish completion file path.
    pub completions_fish: Option<&'a str>,
    /// depends_on directives (e.g. `cask: "other-app"`).
    pub depends_on: &'a [String],
    /// Names of the casks this cask conflicts with, unescaped. Rendered as
    /// one `conflicts_with cask: …` stanza; build via [`conflicting_casks`].
    pub conflicts_with: &'a [String],
    /// Preflight hook Ruby code. Each hook body is re-indented under its
    /// `do` line, so it may be written with or without leading indentation.
    pub preflight: Option<&'a str>,
    /// Postflight hook Ruby code.
    pub postflight: Option<&'a str>,
    /// Uninstall preflight hook Ruby code.
    pub uninstall_preflight: Option<&'a str>,
    /// Uninstall postflight hook Ruby code.
    pub uninstall_postflight: Option<&'a str>,
    /// Pre-rendered Ruby continuation for the `url` line — empty string when
    /// no [`HomebrewCaskURL`](anodizer_core::config::HomebrewCaskURL) sub-fields
    /// (`using`, `cookies`, `referer`, `headers`, `user_agent`, `data`) are
    /// configured. When non-empty, starts with `,\n      using: …` so it
    /// splices directly after the closing `"` of `url "..."`. Build with
    /// [`render_additional_url_params`] and [`URL_ARGS_INDENT`].
    pub url_extras: &'a str,
    /// Same payload as [`Self::url_extras`], built with
    /// [`ARCH_URL_ARGS_INDENT`] so it lines up under the `url` nested inside
    /// `on_<os> do` and `on_<arch> do`.
    pub url_extras_indented: &'a str,
    /// Multi-platform entries (when set, replaces top-level url/sha256).
    /// Rendered in Homebrew's stanza order whatever order they arrive in:
    /// `on_macos` before `on_linux`, `on_arm` before `on_intel`.
    pub platforms: Vec<CaskPlatformBlock>,
    /// Pre-rendered `generate_completions_from_executable` stanza (without
    /// the trailing newline), placed with the other artifact stanzas. Build
    /// via [`render_generate_completions`] from a
    /// `HomebrewCaskGeneratedCompletions`.
    pub generate_completions: Option<String>,
}

/// Recognised `shell_parameter_format` values. Unknown values fall back
/// to a quoted string so user input round-trips without surprise.
const KNOWN_SHELL_PARAMETER_FORMATS: &[&str] =
    &["arg", "clap", "click", "cobra", "flag", "none", "typer"];

/// The stanza name a `generate_completions_from_executable` line opens with.
const GENERATE_COMPLETIONS_STANZA: &str = "generate_completions_from_executable";

/// Render a `generate_completions_from_executable` directive from config.
///
/// Returns `None` when no executable is configured. The returned string
/// has no trailing newline and no indentation on its first line; its
/// keyword-argument lines are indented to sit under the first argument once
/// the cask places the stanza at its usual two-space indent.
///
/// Output shape:
///
/// ```ruby
/// generate_completions_from_executable "bin/myapp", "completions",
///                                      base_name:              "myapp",
///                                      shell_parameter_format: :cobra,
///                                      shells:                 [:bash, :zsh, :fish, :pwsh]
/// ```
pub fn render_generate_completions(
    g: &anodizer_core::config::HomebrewCaskGeneratedCompletions,
) -> Option<String> {
    let exec = g.executable.as_deref().filter(|s| !s.is_empty())?;

    let mut args: Vec<String> = Vec::with_capacity(1 + g.args.as_deref().map_or(0, |a| a.len()));
    args.push(ruby_string(exec));
    if let Some(extra) = g.args.as_deref() {
        args.extend(extra.iter().map(|a| ruby_string(a)));
    }

    let mut kwargs: Vec<(&str, ArgValue)> = Vec::new();
    if let Some(name) = g.base_name.as_deref().filter(|s| !s.is_empty()) {
        kwargs.push(("base_name", ArgValue::Literal(ruby_string(name))));
    }
    if let Some(fmt) = g
        .shell_parameter_format
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        // Known formats render as Ruby symbols (`:cobra`) — barewords, not
        // string literals, so they pass through unescaped.
        let value = if KNOWN_SHELL_PARAMETER_FORMATS.contains(&fmt) {
            format!(":{fmt}")
        } else {
            ruby_string(fmt)
        };
        kwargs.push(("shell_parameter_format", ArgValue::Literal(value)));
    }
    if let Some(shells) = g.shells.as_deref().filter(|s| !s.is_empty()) {
        let joined = shells
            .iter()
            .map(|s| format!(":{s}"))
            .collect::<Vec<_>>()
            .join(", ");
        kwargs.push(("shells", ArgValue::Literal(format!("[{joined}]"))));
    }

    let mut out = format!("{GENERATE_COMPLETIONS_STANZA} {}", args.join(", "));
    if !kwargs.is_empty() {
        let column = STANZA_INDENT.len() + GENERATE_COMPLETIONS_STANZA.len() + 1;
        out.push_str(",\n");
        out.push_str(&" ".repeat(column));
        out.push_str(&render_keyword_args(column, &kwargs, false));
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Keyword-argument layout shared by `uninstall`, `zap`, `conflicts_with` and
// the `url` options.
// ---------------------------------------------------------------------------

/// Indent of a stanza directly inside `cask "…" do`.
const STANZA_INDENT: &str = "  ";
/// Indent of a line inside a stanza's own `do … end` block.
const BLOCK_INDENT: &str = "    ";

/// A double-quoted Ruby string literal holding `s` verbatim.
fn ruby_string(s: &str) -> String {
    format!("\"{}\"", ruby_escape_str(s))
}

/// The value half of one `key: value` keyword argument.
enum ArgValue {
    /// One line of Ruby source, emitted as written.
    Literal(String),
    /// Ruby source literals for an array. One element renders bare, because
    /// `Cask/ArrayAlphabetization` rejects a single-element array.
    Array(Vec<String>),
    /// `(key, value)` Ruby source pairs for a `{ key => value }` hash.
    Hash(Vec<(String, String)>),
}

/// Render `key: value` pairs as the arguments of one call, the first pair at
/// `column` and every later pair on its own line at that same column.
///
/// The layout is what Homebrew's RuboCop settings ask of a multi-line
/// keyword hash: values lined up in one column after the longest key
/// (`Layout/HashAlignment: table`), and a multi-line array or hash value
/// whose elements sit two spaces in from its key and whose closing bracket
/// lines up with the key. The one exception is a lone pair that shares the
/// stanza's own line (`zap trash: [`): RuboCop measures its elements from the
/// start of that line, so `first_shares_stanza_line` says the opening pair
/// does.
///
/// The returned text starts with the first key, so the caller places the
/// stanza name (and any positional arguments) before it.
fn render_keyword_args(
    column: usize,
    pairs: &[(&str, ArgValue)],
    first_shares_stanza_line: bool,
) -> String {
    let key_width = pairs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
            out.push_str(&" ".repeat(column));
        }
        let base = if i == 0 && first_shares_stanza_line && pairs.len() == 1 {
            STANZA_INDENT.len()
        } else {
            column
        };
        out.push_str(key);
        out.push(':');
        out.push_str(&" ".repeat(key_width - key.len() + 1));
        out.push_str(&render_arg_value(value, base));
    }
    out
}

/// Ruby source for one keyword value; a multi-line value puts its elements
/// at `base + 2` and its closing bracket at `base`.
fn render_arg_value(value: &ArgValue, base: usize) -> String {
    let inner = " ".repeat(base + 2);
    let close = " ".repeat(base);
    match value {
        ArgValue::Literal(lit) => lit.clone(),
        ArgValue::Array(items) if items.len() == 1 => items[0].clone(),
        ArgValue::Array(items) => {
            let mut out = String::from("[\n");
            for item in items {
                out.push_str(&format!("{inner}{item},\n"));
            }
            out.push_str(&close);
            out.push(']');
            out
        }
        ArgValue::Hash(entries) => {
            let key_width = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
            let mut out = String::from("{\n");
            for (k, v) in entries {
                let pad = " ".repeat(key_width - k.len());
                out.push_str(&format!("{inner}{k}{pad} => {v},\n"));
            }
            out.push_str(&close);
            out.push('}');
            out
        }
    }
}

/// String literals for an array that `Cask/ArrayAlphabetization` checks,
/// in the order it requires: by each element's source line, ignoring case.
fn sorted_string_literals(items: &[String]) -> Vec<String> {
    let mut literals: Vec<String> = items.iter().map(|s| ruby_string(s)).collect();
    literals.sort_by_key(|lit| format!("{lit},").to_lowercase());
    literals
}

/// Render `<stanza> key: value, …` from string-list pairs, skipping empty
/// lists. Returns `""` when every list is empty.
fn render_list_stanza(stanza: &str, pairs: &[(&str, &[String])]) -> String {
    let args: Vec<(&str, ArgValue)> = pairs
        .iter()
        .filter(|(_, items)| !items.is_empty())
        .map(|(key, items)| (*key, ArgValue::Array(sorted_string_literals(items))))
        .collect();
    if args.is_empty() {
        return String::new();
    }
    let column = STANZA_INDENT.len() + stanza.len() + 1;
    format!("{stanza} {}", render_keyword_args(column, &args, true))
}

// ---------------------------------------------------------------------------
// uninstall / zap / conflicts_with rendering.
// ---------------------------------------------------------------------------

/// The `uninstall` / `zap` directives present in `u`, in the order
/// `Cask/UninstallMethodsOrder` requires.
fn uninstall_like_pairs(
    u: &anodizer_core::config::HomebrewCaskUninstall,
) -> Vec<(&str, &[String])> {
    [
        ("launchctl", &u.launchctl),
        ("quit", &u.quit),
        ("login_item", &u.login_item),
        ("delete", &u.delete),
        ("trash", &u.trash),
    ]
    .into_iter()
    .filter_map(|(key, items)| items.as_deref().map(|items| (key, items)))
    .collect()
}

/// Render the full `uninstall <key>: …, <key>: …` stanza from structured
/// uninstall config. Returns `""` when no uninstall directive is configured.
pub(super) fn render_uninstall_block(
    uninstall: Option<&anodizer_core::config::HomebrewCaskUninstall>,
) -> String {
    uninstall.map_or_else(String::new, |u| {
        render_list_stanza("uninstall", &uninstall_like_pairs(u))
    })
}

/// Render the full `zap <key>: …, <key>: …` stanza from structured zap
/// config. Returns `""` when no zap directive is configured.
pub(super) fn render_zap_block(
    zap: Option<&anodizer_core::config::HomebrewCaskUninstall>,
) -> String {
    zap.map_or_else(String::new, |u| {
        render_list_stanza("zap", &uninstall_like_pairs(u))
    })
}

/// The conflicting cask names from structured conflict config. A `formula:`
/// entry is left out: Homebrew's cask `conflicts_with` accepts only `cask:`,
/// and a cask naming a formula conflict fails to load.
pub(super) fn conflicting_casks(
    conflicts: Option<&[anodizer_core::config::HomebrewCaskConflictEntry]>,
) -> Vec<String> {
    conflicts
        .unwrap_or_default()
        .iter()
        .filter_map(|c| c.cask.clone())
        .filter(|c| !c.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// additional_url_params
// ---------------------------------------------------------------------------

/// Render Ruby kwargs for the `url "<url>"` line: `using`, `cookies`,
/// `referer`, `header`, `user_agent`, `data`. Returns `""` when every
/// sub-field is unset.
///
/// `indent` is the column of the `url` stanza's first argument
/// ([`URL_ARGS_INDENT`] at top level, [`ARCH_URL_ARGS_INDENT`] inside the
/// `on_<os>` / `on_<arch>` blocks); every kwarg line starts there. The
/// returned string starts with `,\n` + indent so it splices directly after
/// the closing `"` of `url "..."`.
pub(super) fn render_additional_url_params(
    u: &anodizer_core::config::HomebrewCaskURL,
    indent: &str,
) -> String {
    // Keys are sorted so output is deterministic across runs (HashMap iter
    // order would otherwise produce noisy diffs in committed cask files).
    fn hash_entries(map: &std::collections::HashMap<String, String>) -> Vec<(String, String)> {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        keys.into_iter()
            .map(|k| (ruby_string(k), ruby_string(&map[k])))
            .collect()
    }

    let mut args: Vec<(&str, ArgValue)> = Vec::new();
    if let Some(v) = u.using.as_deref().filter(|v| !v.is_empty()) {
        // `using:` accepts either a Ruby symbol (`:homebrew_curl`) or a
        // class name (no quotes). It is raw Ruby, not a string literal, so it
        // passes through verbatim without quoting or escaping.
        args.push(("using", ArgValue::Literal(v.to_string())));
    }
    if let Some(cookies) = u.cookies.as_ref().filter(|c| !c.is_empty()) {
        args.push(("cookies", ArgValue::Hash(hash_entries(cookies))));
    }
    if let Some(v) = u.referer.as_deref().filter(|v| !v.is_empty()) {
        args.push(("referer", ArgValue::Literal(ruby_string(v))));
    }
    if let Some(headers) = u.headers.as_ref().filter(|h| !h.is_empty()) {
        // Header order is the request's order, so it is kept as written; a
        // single header still renders as an array because `header:` is not a
        // stanza `Cask/ArrayAlphabetization` checks.
        let literals: Vec<String> = headers.iter().map(|h| ruby_string(h)).collect();
        let value = if literals.len() == 1 {
            ArgValue::Literal(format!("[{}]", literals[0]))
        } else {
            ArgValue::Array(literals)
        };
        args.push(("header", value));
    }
    if let Some(v) = u.user_agent.as_deref().filter(|v| !v.is_empty()) {
        args.push(("user_agent", ArgValue::Literal(ruby_string(v))));
    }
    if let Some(data) = u.data.as_ref().filter(|d| !d.is_empty()) {
        args.push(("data", ArgValue::Hash(hash_entries(data))));
    }

    if args.is_empty() {
        return String::new();
    }
    format!(
        ",\n{indent}{}",
        render_keyword_args(indent.len(), &args, false)
    )
}

/// Homebrew's cask token: lowercase, with spaces as dashes.
///
/// The token names the file (`Casks/<token>.rb`) and opens the cask body, so
/// it must match what `brew` derives from the same name — an unnormalised
/// filename ships a cask `brew` cannot resolve by token.
pub(crate) fn cask_name_for(name: &str) -> String {
    name.replace(' ', "-").to_lowercase()
}

/// Split rendered `alternative_names` into two groups:
///
/// - **Aliases**: pure-string alternative names that Homebrew treats as
///   aliases — they render as additional `name "..."` directives INSIDE
///   the primary cask file.
/// - **Versioned files**: alternative names that contain an `@` — these
///   emit a SECOND `.rb` file named after the alt entry (e.g.
///   `myapp@1.2.3.rb`) so users can `brew install myapp@1.2.3` for a
///   downgrade path.
///
/// The split follows `alternative_names:` semantics: the example
/// `myproject@{{ .Version }}` only makes sense as a separate file, since
/// using it as an alias would mean every release overwrites the previous
/// version's record.
pub(super) fn split_alternative_names(
    rendered: &[String],
    cask_name: &str,
) -> (Vec<String>, Vec<String>) {
    let mut aliases: Vec<String> = Vec::new();
    let mut versioned: Vec<String> = Vec::new();
    for entry in rendered {
        let trimmed = entry.trim();
        // A versioned entry becomes its own token and `.rb` filename, so it is
        // normalised like the primary token; an alias only ever renders as a
        // human-readable `name "..."` stanza and keeps the raw spelling.
        let token = cask_name_for(trimmed);
        if trimmed.is_empty() || token == cask_name {
            continue;
        }
        if trimmed.contains('@') {
            versioned.push(token);
        } else {
            aliases.push(trimmed.to_string());
        }
    }
    (aliases, versioned)
}

/// Pre-render every `alternative_names` entry through the user's template
/// engine. A failure surfaces as `Err` so a typo in the template doesn't
/// silently degrade the rendered cask body.
pub(super) fn render_alternative_names(ctx: &Context, raw: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for entry in raw {
        let rendered = ctx.render_template(entry).with_context(|| {
            format!("homebrew cask: render alternative_names entry '{}'", entry)
        })?;
        out.push(rendered);
    }
    Ok(out)
}

/// Re-indent free-form Ruby supplied by the user: drop leading and trailing
/// blank lines, strip the indentation every line shares, and put `indent`
/// before each non-blank line. Relative indentation inside the text is kept,
/// so a body written flush-left in YAML and one already indented for the
/// cask render the same.
fn reindent(text: &str, indent: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let first = lines
        .iter()
        .position(|l| !l.is_empty())
        .unwrap_or(lines.len());
    let last = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(first, |i| i + 1);
    let lines = &lines[first..last];
    let leading = |l: &str| l.len() - l.trim_start_matches([' ', '\t']).len();
    let common = lines
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| leading(l))
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("{indent}{}", &l[common..])
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `Some(text)` when `text` holds anything but whitespace.
fn non_blank(text: Option<&str>) -> Option<&str> {
    text.filter(|t| !t.trim().is_empty())
}

/// Homebrew's stanza order for the `on_<os>` blocks: `on_macos` before
/// `on_linux` (`ON_SYSTEM_METHODS_STANZA_ORDER` in Homebrew's
/// `rubocops/cask/constants/stanza.rb`).
fn os_block_rank(os_block: &str) -> u8 {
    match os_block {
        "macos" => 0,
        "linux" => 1,
        _ => 2,
    }
}

/// Homebrew's stanza order for the `on_<arch>` blocks: `on_arm` before
/// `on_intel`, from the same constant as [`os_block_rank`].
fn arch_block_rank(arch_block: &str) -> u8 {
    match arch_block {
        "arm" => 0,
        "intel" => 1,
        _ => 2,
    }
}

/// The `on_<os> do … end` blocks, in Homebrew's stanza order and with no
/// blank line between them: they belong to one stanza group. `macos_only`
/// holds stanzas (already indented for the block) placed in the `on_macos`
/// block after its arch blocks.
fn render_platform_blocks(
    platforms: &[CaskPlatformBlock],
    url_extras: &str,
    macos_only: &[String],
) -> String {
    let mut platforms: Vec<&CaskPlatformBlock> = platforms.iter().collect();
    platforms.sort_by_key(|p| os_block_rank(&p.os_block));
    let mut lines: Vec<String> = Vec::new();
    for plat in platforms {
        lines.push(format!("  on_{} do", plat.os_block));
        let mut arches: Vec<&CaskArchEntry> = plat.arches.iter().collect();
        arches.sort_by_key(|a| arch_block_rank(&a.arch_block));
        for arch in arches {
            lines.push(format!("    on_{} do", arch.arch_block));
            lines.push(format!("      sha256 {}", ruby_string(&arch.sha256)));
            lines.push(format!("      url \"{}\"{url_extras}", arch.url));
            lines.push("    end".to_string());
        }
        if plat.os_block == "macos" && !macos_only.is_empty() {
            lines.push(String::new());
            lines.extend(macos_only.iter().cloned());
        }
        lines.push("  end".to_string());
    }
    lines.join("\n")
}

/// A `<name> do … end` stanza around a user-supplied Ruby body.
fn render_do_block(name: &str, body: &str) -> String {
    format!(
        "{STANZA_INDENT}{name} do\n{}\n{STANZA_INDENT}end",
        reindent(body, BLOCK_INDENT)
    )
}

/// Generate a Homebrew Cask Ruby file string from the given parameters.
///
/// The body follows Homebrew's stanza order and grouping
/// (`STANZA_GROUPS` in `rubocops/cask/constants/stanza.rb`): stanzas of one
/// group sit on consecutive lines, groups are separated by exactly one blank
/// line, and nothing precedes the closing `end`. That is the layout
/// `brew style` accepts.
pub fn generate_cask(params: &CaskParams<'_>) -> Result<String> {
    let has_platforms = !params.platforms.is_empty();
    // A cask whose downloads include Linux spans both systems; any other
    // cask downloads only macOS artifacts. `Homebrew/OSDependsOn` wants the
    // first to keep macOS-only stanzas inside `on_macos` and the second to
    // say `depends_on :macos`.
    let cross_platform = params.platforms.iter().any(|p| p.os_block == "linux")
        && params.platforms.iter().any(|p| p.os_block == "macos");
    let app = params
        .app
        .filter(|a| !a.is_empty())
        .map(|a| format!("app {}", ruby_string(a)));
    let service = non_blank(params.service).map(|s| format!("service {}", ruby_string(s.trim())));
    let mut groups: Vec<String> = Vec::new();

    if let Some(block) = non_blank(params.custom_block) {
        groups.push(reindent(block, STANZA_INDENT));
    }

    let mut version = vec![format!("  version {}", ruby_string(params.version))];
    if !has_platforms {
        version.push(format!("  sha256 {}", ruby_string(params.sha256)));
    }
    groups.push(version.join("\n"));

    if has_platforms {
        let nested: Vec<String> = if cross_platform {
            [&app, &service]
                .into_iter()
                .flatten()
                .map(|l| format!("{BLOCK_INDENT}{l}"))
                .collect()
        } else {
            Vec::new()
        };
        groups.push(render_platform_blocks(
            &params.platforms,
            params.url_extras_indented,
            &nested,
        ));
    }

    let mut identity: Vec<String> = Vec::new();
    if !has_platforms {
        identity.push(format!("  url \"{}\"{}", params.url, params.url_extras));
    }
    identity.push(format!("  name {}", ruby_string(params.display_name)));
    for alt in params.alternative_names {
        identity.push(format!("  name {}", ruby_string(alt)));
    }
    if let Some(desc) = params.description.filter(|d| !d.is_empty()) {
        identity.push(format!("  desc {}", ruby_string(desc)));
    }
    if let Some(homepage) = params.homepage.filter(|h| !h.is_empty()) {
        identity.push(format!("  homepage {}", ruby_string(homepage)));
    }
    groups.push(identity.join("\n"));

    // `None` keeps the default skip line; `Some(body)` is the pre-rendered
    // stanza body built via `formula::render_livecheck`.
    let livecheck_body = params
        .livecheck
        .as_deref()
        .unwrap_or("skip \"Auto-generated on release.\"");
    groups.push(format!(
        "  livecheck do\n{}\n  end",
        reindent(livecheck_body, BLOCK_INDENT)
    ));

    let mut relations: Vec<String> = Vec::new();
    let conflicts = render_list_stanza("conflicts_with", &[("cask", params.conflicts_with)]);
    if !conflicts.is_empty() {
        relations.push(format!("{STANZA_INDENT}{conflicts}"));
    }
    for dep in params.depends_on {
        relations.push(format!("  depends_on {dep}"));
    }
    if !cross_platform {
        relations.push("  depends_on :macos".to_string());
    }
    if !relations.is_empty() {
        groups.push(relations.join("\n"));
    }

    let mut artifacts: Vec<String> = Vec::new();
    if let Some(app) = app.as_deref().filter(|_| !cross_platform) {
        artifacts.push(format!("{STANZA_INDENT}{app}"));
    }
    for binary in params.binaries {
        let mut line = format!("  binary {}", ruby_string(&binary.name));
        if let Some(target) = binary.target.as_deref().filter(|t| !t.is_empty()) {
            line.push_str(&format!(", target: {}", ruby_string(target)));
        }
        artifacts.push(line);
    }
    for manpage in params.manpages {
        artifacts.push(format!("  manpage {}", ruby_string(manpage)));
    }
    for (stanza, path) in [
        ("bash_completion", params.completions_bash),
        ("zsh_completion", params.completions_zsh),
        ("fish_completion", params.completions_fish),
    ] {
        if let Some(path) = path {
            artifacts.push(format!("  {stanza} {}", ruby_string(path)));
        }
    }
    // Homebrew runs a cask's artifacts in a fixed order by kind, so this
    // stanza's place in the file does not decide when completions are
    // generated: it always runs before `postflight`.
    if let Some(directive) = params.generate_completions.as_deref() {
        artifacts.push(format!("{STANZA_INDENT}{directive}"));
    }
    if let Some(service) = service.as_deref().filter(|_| !cross_platform) {
        artifacts.push(format!("{STANZA_INDENT}{service}"));
    }
    if !artifacts.is_empty() {
        groups.push(artifacts.join("\n"));
    }

    for (stanza, body) in [
        ("preflight", params.preflight),
        ("postflight", params.postflight),
        ("uninstall_preflight", params.uninstall_preflight),
        ("uninstall_postflight", params.uninstall_postflight),
    ] {
        if let Some(body) = non_blank(body) {
            groups.push(render_do_block(stanza, body));
        }
    }

    for block in [params.uninstall_block, params.zap_block] {
        if !block.is_empty() {
            groups.push(format!("{STANZA_INDENT}{block}"));
        }
    }

    if let Some(caveats) = non_blank(params.caveats) {
        groups.push(format!(
            "  caveats <<~EOS\n{}\n  EOS",
            reindent(caveats, BLOCK_INDENT)
        ));
    }

    Ok(format!(
        "{}\ncask {} do\n{}\nend\n",
        crate::util::GENERATED_FILE_HEADER,
        ruby_string(params.name),
        groups.join("\n\n")
    ))
}

#[cfg(test)]
mod layout_tests {
    use super::reindent;

    #[test]
    fn reindent_puts_a_flush_left_body_under_its_block() {
        assert_eq!(
            reindent("system_command \"/usr/bin/true\"\n", "    "),
            "    system_command \"/usr/bin/true\""
        );
    }

    #[test]
    fn reindent_keeps_relative_indentation_and_drops_outer_blank_lines() {
        let body = "\n      if true\n        x\n\n      end   \n\n";
        assert_eq!(reindent(body, "    "), "    if true\n      x\n\n    end");
    }

    #[test]
    fn a_body_already_indented_for_the_cask_renders_unchanged() {
        let body = "    one\n      two";
        assert_eq!(reindent(body, "    "), body);
    }
}
