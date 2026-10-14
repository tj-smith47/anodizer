use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::super::{Amd64Variant, StringOrBool, deserialize_string_or_bool_opt};
use super::{CommitAuthorConfig, RepositoryConfig};

// ---------------------------------------------------------------------------
// HomebrewConfig / ScoopConfig / TapConfig / BucketConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewConfig {
    /// The tap repository: owner and name, plus branch, token, pull request
    /// and git SSH settings. Replaces the older `tap:` key, which took only
    /// an owner and a name and is no longer accepted.
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Formula directory in the tap (e.g. "Formula").
    pub directory: Option<String>,
    /// Override the formula name (default: crate name).
    pub name: Option<String>,
    /// Short description of the formula (shown in `brew info`).
    pub description: Option<String>,
    /// SPDX license identifier (e.g., "MIT", "Apache-2.0").
    pub license: Option<String>,
    /// Ruby `install` block content for the formula.
    pub install: Option<String>,
    /// Additional install commands appended after the main install block.
    pub extra_install: Option<String>,
    /// Post-install commands (separate `def post_install` block in formula).
    pub post_install: Option<String>,
    /// Ruby `test` block content for the formula (run by `brew test`).
    pub test: Option<String>,
    /// Project homepage URL. Falls back to the GitHub release URL when unset.
    pub homepage: Option<String>,
    /// Package dependencies (e.g. `openssl`, `libgit2`).
    pub dependencies: Option<Vec<HomebrewDependency>>,
    /// Conflicting formula names with optional reason.
    pub conflicts: Option<Vec<HomebrewConflict>>,
    /// Post-install user-facing notes shown by `brew info`.
    pub caveats: Option<String>,
    /// Skip publishing the formula.  `"true"` always skips; `"auto"` skips
    /// for prerelease versions. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Custom commit message template. Rendered via Tera with the standard
    /// release template variables (`ProjectName`, `Tag`, `Version`, etc.).
    /// Default: `"Brew formula update for {{ ProjectName }} version {{ Tag }}"`.
    pub commit_msg_template: Option<String>,
    // Legacy flat `commit_author_name` / `commit_author_email` fields are
    // gone; use the structured `commit_author: { name, email, signing }`.
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// HTTP headers to include in download requests (e.g. for private repos).
    pub url_headers: Option<Vec<String>>,
    /// Custom download strategy class name (e.g.
    /// `:using => GitHubPrivateRepositoryReleaseDownloadStrategy`).
    pub download_strategy: Option<String>,
    /// Ruby `require` statement for custom download strategies.
    pub custom_require: Option<String>,
    /// Custom Ruby code block inserted into the formula class body.
    pub custom_block: Option<String>,
    /// Launchd plist content for `brew services`.
    pub plist: Option<String>,
    /// Homebrew service block content (alternative to plist).
    pub service: Option<String>,
    /// Manpage file paths to install into the formula's `man1` (e.g.
    /// `["mytool.1"]`). Each entry renders a `man1.install "<path>"` line in
    /// the install block, mirroring real Rust-CLI formulae (ripgrep, fd, bat).
    /// A path ending in `.N` (where N is 1–8) routes to the matching `manN`
    /// section; anything else defaults to `man1`.
    pub manpages: Option<Vec<String>>,
    /// Prebuilt shell-completion file paths to install. When set, the formula
    /// emits `bash_completion.install "<path>"` / `zsh_completion.install` /
    /// `fish_completion.install` in its install block — the form used when the
    /// archive ships ready-made completion files.
    pub completions: Option<HomebrewCaskCompletions>,
    /// Generate completions by running the installed binary at install time.
    /// Renders the modern homebrew-core idiom
    /// `generate_completions_from_executable(bin/"<exe>", ...)` in the install
    /// block. Preferred over `completions` when the binary can emit its
    /// own completions; the two are independent and may both be set.
    pub generate_completions_from_executable: Option<HomebrewCaskGeneratedCompletions>,
    /// `livecheck` stanza configuration for the formula. When unset, a binary
    /// tap formula emits `livecheck { skip "Auto-generated on release." }` to
    /// match the cask (the archive URL/sha are rewritten on every release, so
    /// `brew livecheck` cannot meaningfully poll). Set `strategy:` /
    /// `regex:`/`url:` to opt into active version detection instead.
    pub livecheck: Option<HomebrewLivecheck>,
    /// Homebrew Cask for this crate, the same setting as
    /// `publish.homebrew_cask:` (set one of the two). The cask is written into
    /// this formula's tap in the same commit, unless its own `repository:`
    /// names a different tap.
    pub cask: Option<HomebrewCaskConfig>,
    /// amd64 microarchitecture variant filter (`v1` / `v2` / `v3` / `v4`).
    /// Only artifacts matching this variant are included. Default: `v1`.
    /// Typed as [`Amd64Variant`], so any value outside `v1`..`v4` is
    /// rejected when the config is parsed.
    pub amd64_variant: Option<Amd64Variant>,
    /// **Deprecated.** Ignored. Homebrew has no 32-bit ARM variant, so a
    /// formula leaves out every 32-bit ARM archive whatever this says. The
    /// field still parses so existing configs keep loading, and a
    /// deprecation warning names it at config-load time.
    pub arm_variant: Option<String>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the Homebrew publisher is
    /// skipped. Render failure hard-errors. Config key: `brews[].if:`.
    /// An absent, empty or blank `if:` imposes no gate and always runs; the
    /// falsy test applies to what a non-blank gate renders.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place and does not try to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewDependency {
    /// Homebrew formula name of the dependency. The formula lists its
    /// dependencies by type (`build`, `test`, runtime, `recommended`,
    /// `optional`) and within a type by name without regard to case, the
    /// order `brew style` requires; the names are written as given.
    pub name: String,
    /// Restrict to a specific OS: `"mac"` or `"linux"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Dependency type, rendered as `depends_on "name" => :type`:
    /// `"build"`, `"test"`, `"recommended"` or `"optional"`. Takes
    /// precedence over `version`.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub dep_type: Option<String>,
    /// Version constraint for the dependency (e.g. `">= 1.1"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A Homebrew conflict entry, supporting both a bare name string and a
/// structured object with an optional `because` reason.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(untagged)]
pub enum HomebrewConflict {
    /// Just the formula name (e.g. `"other-tool"`).
    Name(String),
    /// Name with reason (e.g. `{name: "other-tool", because: "both install a bin/foo binary"}`).
    WithReason {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        because: Option<String>,
    },
}

impl HomebrewConflict {
    pub fn name(&self) -> &str {
        match self {
            Self::Name(n) => n,
            Self::WithReason { name, .. } => name,
        }
    }
    pub fn because(&self) -> Option<&str> {
        match self {
            Self::Name(_) => None,
            Self::WithReason { because, .. } => because.as_deref(),
        }
    }
}

/// `livecheck` stanza configuration for a Homebrew formula.
///
/// Default (the struct absent from config): the formula emits a
/// `livecheck { skip "Auto-generated on release." }` block — correct for a
/// binary tap whose archive URL/sha256 are rewritten every release. Setting
/// any of `strategy`, `url` or `regex` turns on active version polling:
///
/// ```yaml
/// livecheck:
///   strategy: github_latest
/// ```
///
/// renders:
///
/// ```ruby
/// livecheck do
///   url :stable
///   strategy :github_latest
/// end
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewLivecheck {
    /// `true` forces the `skip "<reason>"` form even when `strategy`, `url`
    /// or `regex` is set. When unset or `false`, the livecheck is active
    /// when any of `strategy`, `url` or `regex` is set, and takes the skip
    /// form otherwise; `false` with none of the three set also logs a
    /// warning.
    pub skip: Option<bool>,
    /// Reason text for the `skip "<reason>"` form.
    /// Default: `"Auto-generated on release."`.
    pub skip_reason: Option<String>,
    /// `livecheck` strategy symbol (e.g. `github_latest`, `git`, `page_match`).
    /// Rendered as a Ruby symbol: `strategy :github_latest`.
    pub strategy: Option<String>,
    /// `url` for the livecheck. Accepts a Ruby symbol shorthand
    /// (`stable` / `head` / `homepage`, and `url` for a cask → `url :stable`)
    /// or a literal URL string (`url "https://..."`). Defaults to `:stable`
    /// in an active livecheck that sets no url.
    pub url: Option<String>,
    /// `regex(...)` argument for `page_match`-style strategies. Emitted
    /// verbatim inside `regex(...)`, so it is raw Ruby (e.g. `%r{v(\d+\.\d+)}i`).
    pub regex: Option<String>,
}

/// Homebrew Cask configuration, with the same keys in two places:
/// - `homebrew_casks:`: a top-level list; carries `repository`,
///   `commit_author`, `directory`, `ids`, `url`, structured `uninstall`/`zap`, etc.
/// - `crates[].publish.homebrew_cask:`: a per-crate cask, where
///   `url_template` is the simpler alternative to `url`.
///
/// Every key is optional in both places.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskConfig {
    // ----- Identity -----
    /// Cask name (default: crate / project name).
    pub name: Option<String>,
    /// Alternative cask names (aliases).
    pub alternative_names: Option<Vec<String>>,

    // ----- Tap repository (top-level axis) -----
    /// Unified repository config for the Homebrew tap.
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template.
    /// Default: "Brew cask update for {{ ProjectName }} version {{ Tag }}"
    pub commit_msg_template: Option<String>,
    /// Subdirectory in the tap repo for cask placement (default: "Casks").
    pub directory: Option<String>,

    // ----- Artifact selection -----
    /// Build IDs filter: only include artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,

    // ----- Download URL -----
    /// Simple URL template for the .dmg/.zip download (per-crate shorthand).
    ///
    /// Cannot be combined with `url.template:` — set one or the other.
    /// If both are present, config validation rejects the config at parse time.
    /// Use `url:` for the structured form (verified domain, custom headers, etc.)
    /// or `url_template:` for a bare string shorthand — never both simultaneously.
    pub url_template: Option<String>,
    /// Structured download URL configuration (top-level axis).
    pub url: Option<HomebrewCaskURL>,

    // ----- macOS bundle -----
    /// macOS .app bundle name (e.g. "MyApp.app").
    pub app: Option<String>,
    /// Binary stubs to create in /usr/local/bin.
    ///
    /// Each entry is either a bare string (`"my-cli"` → emits
    /// `binary "my-cli"`) or a structured `{ name, target }` object
    /// (`{ name: "my-cli", target: "mycli" }` → emits
    /// `binary "my-cli", target: "mycli"`). The `target:` form mirrors
    /// the Homebrew Ruby cask DSL for binary renames — without it, a
    /// wrapped binary installs at the wrong path.
    /// Cask binary entry.
    pub binaries: Option<Vec<HomebrewCaskBinary>>,
    /// Deprecated singular spelling of [`Self::binaries`]. The upstream
    /// replaced `binary: foo` with `binaries: [foo]`; this field captures the
    /// legacy spelling so imported configs keep parsing.
    /// [`apply_homebrew_cask_legacy_singulars`](super::super::apply_homebrew_cask_legacy_singulars)
    /// folds the value into [`Self::binaries`] at config-load time and emits
    /// a one-time deprecation warning per occurrence. The field is excluded
    /// from serialization so a round-tripped config emits only the canonical
    /// plural form.
    #[serde(default, rename = "binary", skip_serializing)]
    pub legacy_binary: Option<String>,

    // ----- Metadata -----
    /// Cask description.
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// License identifier (SPDX).
    pub license: Option<String>,
    /// Custom caveats shown after install.
    pub caveats: Option<String>,

    // ----- Ruby block -----
    /// Arbitrary Ruby code inserted into the cask block.
    pub custom_block: Option<String>,
    /// Path of a macOS Services bundle (for example `MyTool.workflow`) inside
    /// the download, written as `service "<path>"`.
    pub service: Option<String>,

    // ----- Livecheck -----
    /// `livecheck` stanza configuration for the cask. When unset, the cask
    /// emits `livecheck do\n  skip "Auto-generated on release."\nend` (a
    /// binary cask's download URL/sha256 are rewritten on every release, so
    /// `brew livecheck` has nothing stable to poll). Set `strategy:` /
    /// `url:` / `regex:` (with `skip: false`) to opt into active version
    /// detection — the same shape a Homebrew cask `livecheck do … end`
    /// block accepts. Reuses the formula `livecheck` config type.
    pub livecheck: Option<HomebrewLivecheck>,

    // ----- Completions / manpages -----
    /// Manual page references to install.
    pub manpages: Option<Vec<String>>,
    /// Deprecated singular spelling of [`Self::manpages`]. The upstream replaced
    /// `manpage: foo.1` with `manpages: [foo.1]`; this field captures the
    /// legacy spelling so imported configs keep parsing.
    /// [`apply_homebrew_cask_legacy_singulars`](super::super::apply_homebrew_cask_legacy_singulars)
    /// folds the value into [`Self::manpages`] at config-load time and emits
    /// a one-time deprecation warning per occurrence. The field is excluded
    /// from serialization so a round-tripped config emits only the canonical
    /// plural form.
    #[serde(default, rename = "manpage", skip_serializing)]
    pub legacy_manpage: Option<String>,
    /// Shell completion definitions.
    pub completions: Option<HomebrewCaskCompletions>,
    /// Auto-generate shell completions from an executable.
    pub generate_completions_from_executable: Option<HomebrewCaskGeneratedCompletions>,

    // ----- Dependencies / conflicts -----
    /// Cask dependencies (other casks or formulae). Rendered casks first,
    /// then formulae, each group sorted by name without regard to case
    /// with the names written as given, the order `brew style` requires.
    pub dependencies: Option<Vec<HomebrewCaskDependencyEntry>>,
    /// Conflicting casks or formulae.
    pub conflicts: Option<Vec<HomebrewCaskConflictEntry>>,
    /// Whether the cask renders `depends_on :macos`. When unset, anodizer
    /// decides from the downloads and the `custom_block`: a cask with no
    /// Linux download gets `depends_on :macos` unless its `custom_block`
    /// gives it a Linux side or declares an OS dependency.
    ///
    /// The block is read as code, with these set aside, each one carrying on
    /// into the next lines until it closes: `"…"`, `'…'` and `` `…` ``
    /// strings (inside `"…"` and `` `…` ``, `#{…}` is read as code, so the
    /// string ends at its own quote, and `#$"` interpolates `$"`); heredoc
    /// bodies (`<<~TAG`, `<<-TAG`, `<<TAG`, a `<<` before a space or a digit
    /// being a shift; unless the tag is single-quoted, a body line ending in
    /// an odd run of `\` joins the next one, so a tag there does not close
    /// it, and `#{…}` in a body line is code, so a heredoc opened there has
    /// its body read from the next line, before the rest of the body around
    /// it, and a `#{…}` left open at the end of a body line reads on into the
    /// next); `%()`, `%q`, `%Q`, `%w`, `%W`, `%i`, `%I`, `%r`, `%s` and `%x`
    /// literals delimited by nesting brackets or one repeated punctuation
    /// character, a typed one also by a space or the end of the line (a list,
    /// `%w` `%W` `%i` `%I`, splits its words on whitespace, so whitespace
    /// never closes it), and a bare `%` where an operand must stand (with
    /// nothing before it on its statement but an operator, or after a label
    /// `a:`, the `:` of a ternary or a keyword that takes an operand: `and`,
    /// `begin`, `break`, `case`, `do`, `else`, `elsif`, `ensure`, `if`, `in`,
    /// `next`, `or`, `rescue`, `return`, `then`, `unless`, `until`, `when` or
    /// `while`) also by a space, a tab, `=` or the end of the line (a literal
    /// delimited by the end of its line holds the next line, and the one
    /// after while the held line ends in an odd run of `\`, and a `%r` closed
    /// that way takes the letters that start the line after as its options);
    /// `?x` character literals (not `?ab`, nor `?"a"` after a word that may
    /// be a local); a `/…/` regex where an operand stands; `#` comments; and
    /// `=begin` … `=end` comments, opened on any line that starts in code,
    /// the code of a `#{…}` left open at the end of the line before included.
    /// The code inside `#{…}` is kept apart from the code around it, in every
    /// literal that interpolates (`"…"` and `` `…` `` strings, `:"…"`
    /// symbols, heredocs whose tag is not single-quoted, `%()`, `%Q`, `%W`,
    /// `%I`, `%r` and `%x` literals and `/…/` regexes), and each `#{…}` is
    /// read as statements of its own: its words count toward the Linux side,
    /// a `depends_on` opening one of its statements toward the OS dependency,
    /// and a `;` in it ends no statement around the literal; it is read in
    /// the scopes around the literal by the rules below, as Ruby reads it, so
    /// the names it assigns are locals after it and the parameters and locals
    /// of a block, lambda or `def` inside it end with that block, lambda or
    /// `def` (`"#{on_linux}"` names a Linux side, `'#{on_linux}'` does not).
    /// None opens after a `:` that makes a symbol (one written against a
    /// method call that heads its statement or follows `.`, `&.` or `::`
    /// does, as in `` foo:` ``, since such a call takes no label), in a
    /// special global (`$"`), or where it names a method: after `.`, `&.` or
    /// `::` (with or without a space, whatever the name looks like, so `x. %`
    /// and `a&.!~` are method names) or `def`, and as the `?` or `!` of a
    /// method name. Straight after a name Ruby reads as a method call, a `?`
    /// opens a character literal (`EOS!?"`, `foo?=`, `x&.def??"`,
    /// `foo.nil?=`). Ruby reads a name as a method call after `.`, `&.` or
    /// `::`, whatever it spells, and elsewhere unless it is a local variable,
    /// a parameter, a number, a variable with a sigil, or `self`, `nil`,
    /// `true`, `false`, `end`, `__FILE__`, `__LINE__` or `__ENCODING__`. A
    /// `?` or `!` read as an operator ends no word, so a `/` after it opens a
    /// regex (`!/a/`).
    ///
    /// A `<<`, `%`, `?` or `/` after a value is a shift, a modulo, a ternary
    /// or a division: after `)`, `]`, `}`, a number, a literal, a symbol (the
    /// operator ones and `:a=`, `:a?`, `:a!` included), `self`, `nil`,
    /// `true`, `false`, `end`, `class`, `def`, an `@` or `$` variable, a
    /// local assigned earlier in its scope (`name =`, `name ||=` and the
    /// other operator assignments wherever they stand, as in `f(b += 1)`,
    /// unless the target follows `.`, `&.` or `::` or has a sigil, `a, b =`,
    /// `a, (b, c) =`, `(a, b) =`, `a, =`, `for a in`, `rescue => a`, a named
    /// group before `=~` in a regex with no `#{…}`, a name in an `in`
    /// pattern, the target of a rightward `=>`), a parameter of a block
    /// (`|a, b; c|`, also over several lines), method (`def name(a, b = 1)`,
    /// with or without parentheses; a setter `def name=(v)` binds `v` alone)
    /// or lambda (`->(x)`, `-> (x)`), or a word other than a keyword written
    /// straight against a `<<`, `%` or `/`. A `def` body sees none of the
    /// locals outside it; a block, a lambda body (`-> { … }`) included, sees
    /// them, and its parameters and the locals first assigned in it end at
    /// its `end` or `}`; `if`, `while`, `case` and `begin` open no scope. A
    /// statement continued with a `\` at the end of a line, with a `?\`
    /// character literal holding the newline, with a `.`, `&.` or `::` that
    /// ends a line (past blank and comment lines) or with a literal the
    /// newline closes, is read as one statement, so the code before the break
    /// decides what the operator, `if` or `until` after it is. After a word,
    /// a `/` followed by a space or `=` divides, and a `:` after a value ends
    /// a ternary and starts no symbol, so an operand stands after it. The
    /// word after a `::` is a constant path or a method, also when the `::`
    /// ended an earlier line. After an operator, `(`, `,`, `;`, the start of
    /// a line, a label, a Ruby keyword, any method of the cask DSL, a common
    /// method such as `puts` or a name ending in `?` or `!` followed by a
    /// space (no local or parameter is spelled that way) the operator opens a
    /// literal, and after any other word followed by a space it is read as an
    /// opener too, which is a guess.
    ///
    /// A Linux side is the word `on_linux`, `on_system` or `app_image` in
    /// that code. An OS dependency is a statement whose first word is
    /// `depends_on` (at the start of a line or after a `;`, in the code
    /// around the literals or in a `#{…}`, after any opening `(`) that names
    /// the key `macos:`, `maximum_macos:` or `linux:`, or the symbol `:macos`
    /// or `:linux`; the statement continues onto the next line while a line
    /// ends with `,`, `(`, `[`, `{`, `|`, `.`, `&&`, `||`, `=>` or `\`, ends
    /// inside an open literal, or while it has opened more `(`, `[` and `{`
    /// than it closed.
    ///
    /// The checks fall back to a word scan of the raw text, comments and
    /// strings included, which may over-match, when a literal is still open
    /// after the last line, when a guess opens a literal that spans several
    /// lines, or when a parameter still in scope is followed by a `<<` whose
    /// heredoc tag closes on a later line. A word scan that finds a
    /// Linux-side word leaves `depends_on :macos` out and moves a flat
    /// download into `on_macos`, one that finds a `depends_on` OS key leaves
    /// the bare dependency out, and one that finds neither adds
    /// `depends_on :macos`; the render warns once per command unless this
    /// field is set.
    ///
    /// Read wrongly with no warning: a stanza built at runtime (through
    /// `send` or `define_method`), a `depends_on` after a `do` on the same
    /// line, and a guess whose literal closes on the same line, whose
    /// content is set aside.
    ///
    /// `true` renders `depends_on :macos` with a flat download; `false`
    /// renders no dependency and places a flat download inside `on_macos`.
    pub depends_on_macos: Option<bool>,

    // ----- Lifecycle hooks -----
    /// Pre/post install/uninstall hooks.
    pub hooks: Option<HomebrewCaskHooks>,

    // ----- Uninstall / zap -----
    /// Structured uninstall stanza configuration.
    pub uninstall: Option<HomebrewCaskUninstall>,
    /// Deep uninstall (zap) stanza configuration.
    pub zap: Option<HomebrewCaskUninstall>,

    // ----- Publishing control -----
    /// Skip publishing the cask. `"true"` always skips; `"auto"` skips
    /// for prerelease versions. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// When true, force-push the updated cask file to the existing PR branch
    /// when a PR for the same head branch already exists. The PR content is
    /// updated in place, so no duplicate is created. When false (default),
    /// the push is skipped and a warning is emitted so the operator sees that
    /// the publisher did not update the PR.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub update_existing_pr: Option<StringOrBool>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the Homebrew Cask config is
    /// skipped. Render failure hard-errors. Config key: `homebrew_casks[].if:`.
    /// An absent, empty or blank `if:` imposes no gate and always runs; the
    /// falsy test applies to what a non-blank gate renders.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place and does not try to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}

/// Structured URL configuration for Homebrew Cask downloads.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskURL {
    /// URL template for the download.
    pub template: Option<String>,
    /// **Deprecated.** Verification string (domain shown to user). Homebrew
    /// removed the `verified:` stanza (Homebrew/brew#23280) and now applies
    /// its default URL verification, so anodizer no longer writes it into the
    /// cask. The field still parses so existing configs keep loading, and a
    /// deprecation warning names it at config-load time.
    pub verified: Option<String>,
    /// Custom downloader (e.g. `:homebrew_curl`, `:post`).
    pub using: Option<String>,
    /// HTTP cookies for the download.
    pub cookies: Option<HashMap<String, String>>,
    /// Referer header for the download.
    pub referer: Option<String>,
    /// Custom HTTP headers.
    pub headers: Option<Vec<String>>,
    /// Custom user agent string.
    pub user_agent: Option<String>,
    /// POST data for form submissions.
    pub data: Option<HashMap<String, String>>,
}

/// Structured uninstall/zap configuration for Homebrew Cask.
/// Used for both `uninstall` and `zap` stanzas.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskUninstall {
    /// Launch daemon/agent identifiers to stop.
    pub launchctl: Option<Vec<String>>,
    /// Application bundle IDs to quit.
    pub quit: Option<Vec<String>>,
    /// Login item names to remove.
    pub login_item: Option<Vec<String>>,
    /// File paths to delete.
    pub delete: Option<Vec<String>>,
    /// File paths to trash (preserves app state).
    pub trash: Option<Vec<String>>,
}

/// Pre/post install/uninstall hooks for Homebrew Cask.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskHooks {
    /// Pre-install/uninstall hooks.
    pub pre: Option<HomebrewCaskHook>,
    /// Post-install/uninstall hooks.
    pub post: Option<HomebrewCaskHook>,
}

/// Individual hook for install/uninstall phases.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskHook {
    /// Ruby code for preflight/postflight during install.
    pub install: Option<String>,
    /// Ruby code for uninstall_preflight/uninstall_postflight.
    pub uninstall: Option<String>,
}

/// Shell completion file paths for Homebrew Cask.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskCompletions {
    /// Path to bash completion file.
    pub bash: Option<String>,
    /// Path to zsh completion file.
    pub zsh: Option<String>,
    /// Path to fish completion file.
    pub fish: Option<String>,
}

/// Cask `binary` stanza entry.
///
/// Two shapes accepted in YAML:
/// - bare string — `"my-cli"` → renders `binary "my-cli"`.
/// - `{ name, target }` object — `{ name: "my-cli", target: "mycli" }`
///   → renders `binary "my-cli", target: "mycli"`. The `target:` form is
///   the Homebrew Ruby cask DSL rename: install the symlink at
///   `/usr/local/bin/<target>` in place of `/usr/local/bin/<name>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(untagged)]
pub enum HomebrewCaskBinary {
    /// Bare binary name. Equivalent to `{ name: "<n>", target: None }`.
    Name(String),
    /// Structured `{ name, target }` rename form.
    WithTarget {
        /// Path inside the .app bundle (e.g. `"my-cli"`).
        name: String,
        /// Optional rename target — the symlink name in `/usr/local/bin`.
        /// When unset, the symlink uses `name`.
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
}

impl HomebrewCaskBinary {
    /// The binary name (the path inside the .app bundle).
    pub fn name(&self) -> &str {
        match self {
            Self::Name(n) => n,
            Self::WithTarget { name, .. } => name,
        }
    }
    /// The optional rename target. `None` for bare-string entries and for
    /// `{ name, target }` objects without `target` set.
    pub fn target(&self) -> Option<&str> {
        match self {
            Self::Name(_) => None,
            Self::WithTarget { target, .. } => target.as_deref(),
        }
    }
}

/// Cask dependency (on another cask or formula).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskDependencyEntry {
    /// Dependent cask name.
    pub cask: Option<String>,
    /// Dependent formula name.
    pub formula: Option<String>,
}

/// Cask conflict (with another cask or formula).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskConflictEntry {
    /// Conflicting cask name.
    pub cask: Option<String>,
    /// Deprecated: Homebrew casks can only conflict with casks, so this is
    /// left out of the rendered cask and a deprecation warning is logged.
    pub formula: Option<String>,
}

/// Auto-generate shell completions from an executable.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskGeneratedCompletions {
    /// Binary to generate completions from.
    pub executable: Option<String>,
    /// Arguments to pass to the executable.
    pub args: Option<Vec<String>>,
    /// Base name for completion files.
    pub base_name: Option<String>,
    /// Shell completion framework type (arg, clap, click, cobra, flag, none, typer).
    pub shell_parameter_format: Option<String>,
    /// Target shells (bash, zsh, fish, pwsh).
    pub shells: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ScoopConfig {
    /// The bucket repository: owner and name, plus branch, token, pull
    /// request and git SSH settings. Replaces the older `bucket:` key, which
    /// took only an owner and a name and is no longer accepted.
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Override the manifest name (default: crate name).
    pub name: Option<String>,
    /// Subdirectory in the bucket repo for manifest placement. Defaults to
    /// `bucket` — scoop resolves manifests only from `bucket/` when that
    /// directory exists (root otherwise), so `bucket/` is correct for both
    /// layouts. Set to `""` to target the repo root. A stale same-named
    /// root-level manifest is removed when publishing into a subdirectory.
    pub directory: Option<String>,
    /// Short description of the package (shown in `scoop info`).
    pub description: Option<String>,
    /// SPDX license identifier (e.g., "MIT", "Apache-2.0").
    pub license: Option<String>,
    /// Project homepage URL. Falls back to the GitHub-derived URL when unset.
    pub homepage: Option<String>,
    /// Data paths persisted between Scoop updates.
    pub persist: Option<Vec<String>>,
    /// Application dependencies (other Scoop packages).
    pub depends: Option<Vec<String>>,
    /// Commands to run before installation.
    pub pre_install: Option<Vec<String>>,
    /// Commands to run after installation.
    pub post_install: Option<Vec<String>>,
    /// Start menu shortcuts as `[executable, label]` pairs.
    pub shortcuts: Option<Vec<Vec<String>>>,
    /// Scoop `checkver` strategy used by bucket maintainers to detect new
    /// releases. Defaults to `"github"` (derived from the configured GitHub
    /// repo) — `ScoopInstaller/Main` requires checkver for automated-update
    /// PRs. Override with a homepage regex when GitHub release detection is
    /// not appropriate.
    ///
    /// Example: `checkver: "github"` or `checkver: "v([\\d.]+)"`.
    pub checkver: Option<String>,
    /// Skip publishing the manifest.  `"true"` always skips; `"auto"` skips
    /// for prerelease versions. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Custom commit message template.
    pub commit_msg_template: Option<String>,
    // Use the structured `commit_author: { name, email, signing }` form for
    // commit author identity (legacy flat `commit_author_name` /
    // `commit_author_email` fields are not accepted).
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// Artifact selection: "archive" (default), "msi", or "nsis".
    #[serde(rename = "use")]
    pub use_artifact: Option<String>,
    /// amd64 microarchitecture variant filter (`v1` / `v2` / `v3` / `v4`).
    /// Only artifacts matching this variant are included. Default: `v1`.
    /// Typed as [`Amd64Variant`], so any value outside `v1`..`v4` is
    /// rejected when the config is parsed.
    pub amd64_variant: Option<Amd64Variant>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the Scoop publisher is
    /// skipped. Render failure hard-errors. Config key: `scoop[].if:`.
    /// An absent, empty or blank `if:` imposes no gate and always runs; the
    /// falsy test applies to what a non-blank gate renders.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place and does not try to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}

// `TapConfig` / `BucketConfig` (legacy {owner, name}-only repo types) live
// nowhere — every publisher now carries `repository: RepositoryConfig`
// with the broader feature set (token / branch / git SSH / pull_request).
