use super::*;

/// The values a `binary_signs:` entry's `artifacts:` may take.
///
/// `binary_signs:` signs raw built binaries, so the loader's own
/// `deserialize_binary_signs` refuses everything else on that field. The
/// `defaults.binary_signs:` block is a plain `SignConfig` that the defaults
/// fold copies into the slice without passing through that deserializer, so
/// a wider value reaches the run there and is silently ignored — the sign
/// stage filters binaries whatever the entry says.
const BINARY_SIGN_ARTIFACT_FILTERS: &[&str] = &["binary", "none"];

/// Warn on sign artifact filter values the slice cannot honor.
///
/// The accepted vocabulary is the runtime resolver's own
/// `VALID_SIGN_ARTIFACT_FILTERS` (the source of truth for
/// `should_sign_artifact`), so check-time validation cannot drift behind a
/// value the sign stage actually honors. Every sign slice is asked:
/// `binary_signs:` and the per-crate slices resolve the filter through that
/// same resolver, so a value unrecognized on one of them fails the run just
/// as loudly. A `binary_signs:` slice is asked the narrower question its own
/// field is loaded under.
pub(super) fn check_sign_artifact_filters(config: &Config, warnings: &mut Vec<String>) {
    let valid_artifact_filters = anodizer_stage_sign::VALID_SIGN_ARTIFACT_FILTERS;
    let unrecognized = |filter: &Option<String>| -> Option<String> {
        let filter = filter.as_deref()?;
        (!valid_artifact_filters.contains(&filter)).then(|| filter.to_string())
    };
    for slice in sign_slices(config) {
        for (idx, sign_cfg) in slice.configs.iter().enumerate() {
            let block = slice.block(idx);
            if slice.is_binary_signs()
                && let Some(filter) = sign_cfg.artifacts.as_deref()
                && !BINARY_SIGN_ARTIFACT_FILTERS.contains(&filter)
            {
                warnings.push(format!(
                    "{block} artifacts filter '{filter}' is not allowed on \
                    binary_signs (valid: {}) — the sign stage signs binaries \
                    whatever it says",
                    BINARY_SIGN_ARTIFACT_FILTERS.join(", ")
                ));
            } else if let Some(filter) = unrecognized(&sign_cfg.artifacts) {
                warnings.push(format!(
                    "unrecognized {block} artifacts filter '{filter}' (valid: {})",
                    valid_artifact_filters.join(", ")
                ));
            }
            // The authenticode block carries its own `artifacts` selector,
            // resolved through the same `should_sign_artifact` vocabulary. An
            // unrecognized value here matches no artifact and (now that the
            // stage propagates the error) fails the run — surface it at check
            // time too.
            if let Some(ref auth) = sign_cfg.authenticode
                && let Some(filter) = unrecognized(&auth.artifacts)
            {
                warnings.push(format!(
                    "unrecognized {block} authenticode artifacts filter \
                    '{filter}' (valid: {})",
                    valid_artifact_filters.join(", ")
                ));
            }
        }
    }
}

/// Warn that `asset_name_template:` only names a `binary_signs:` asset.
///
/// A `signs:` signature is named by its `signature:` template alone — its
/// subject is already an asset with a unique name — so the field parses on
/// every `SignConfig` but is read only on the `binary_signs:` slice. A user
/// who sets it on `signs:` gets the derived name with no error at all.
pub(super) fn check_sign_asset_name_templates(config: &Config, warnings: &mut Vec<String>) {
    for slice in sign_slices(config) {
        if slice.is_binary_signs() {
            continue;
        }
        for (idx, cfg) in slice.configs.iter().enumerate() {
            if cfg.asset_name_template.is_none() {
                continue;
            }
            let block = slice.block(idx);
            warnings.push(format!(
                "{block}.asset_name_template is set but only binary_signs \
                honors it (it will be ignored)"
            ));
        }
    }
}

/// Whether two sign entries can select one artifact.
///
/// Three terms, each able to keep the pair apart on its own:
///
/// - `artifacts:` — the kinds each entry signs, resolved by the sign stage's
///   own `should_sign_artifact` through `sign_filters_can_overlap`. On
///   `signs:` this is THE selector: an entry signing archives and one
///   signing the checksum file never meet, whatever they name their output.
///   `artifacts_fallback` is the slice's own default, so an absent filter is
///   read as the run reads it (`"none"` on `signs:`, `"binary"` on
///   `binary_signs:`).
/// - `if:` — two DIFFERENT gates are not evaluated, because a template can
///   read the environment, so "both fire" cannot be decided here, and two
///   entries that never both run write no second file. An ABSENT gate always
///   fires, so it pairs with anything: whenever the gated entry runs, both
///   write. `if: ""` is absent as far as the run is concerned, so it is read
///   through `active_if_gate`, the same answer the engine acts on.
/// - `ids:` — an absent list takes every build, and two present lists overlap
///   when they name an id in common.
fn sign_selections_overlap(
    a: &anodizer_core::config::SignConfig,
    b: &anodizer_core::config::SignConfig,
    artifacts_fallback: &str,
) -> bool {
    use anodizer_core::config::active_if_gate;
    if !anodizer_stage_sign::sign_filters_can_overlap(
        a.resolved_artifacts(artifacts_fallback),
        b.resolved_artifacts(artifacts_fallback),
    ) {
        return false;
    }
    if let (Some(left), Some(right)) = (
        active_if_gate(a.if_condition.as_deref()),
        active_if_gate(b.if_condition.as_deref()),
    ) && left != right
    {
        return false;
    }
    match (&a.ids, &b.ids) {
        (Some(left), Some(right)) => left.iter().any(|id| right.contains(id)),
        _ => true,
    }
}

/// Whether an entry writes a detached signature at all: Authenticode signs
/// the PE in place, and `artifacts: none` signs nothing.
fn writes_detached_outputs(cfg: &anodizer_core::config::SignConfig) -> bool {
    cfg.authenticode.is_none() && cfg.artifacts.as_deref() != Some("none")
}

/// `template`'s `{{ … }}` runs reduced to one opaque path component each.
///
/// The separators inside a run are mapped to an opaque character, which
/// leaves the placeholder one component whatever it holds, so the literal
/// segments AROUND it fold as the components they are. Without that a
/// `{{ printf "a/b" }}` would split into two components and a `..` beside it
/// would climb into the placeholder's own text. The padding just inside the
/// braces is trimmed as well, so `{{ Version }}` and `{{Version}}` — which
/// render identically — are one placeholder.
///
/// The scan is not a template parser, and two malformed spellings answer
/// approximately. An unterminated `{{` masks to the end of the string, so
/// two spellings differing anywhere after it read as two files — the
/// direction a missed advisory warning lies in. A `}}` inside a quoted
/// literal (`{{ printf "}}" }}`) closes the run early instead, which leaves
/// the tail unmasked and its separators read as real path components.
fn mask_placeholder_separators(template: &str) -> String {
    const OPAQUE: char = '\u{1}';
    let hide = |run: &str| -> String {
        run.chars()
            .map(|c| match c {
                '/' | '\\' => OPAQUE,
                other => other,
            })
            .collect()
    };
    let mut masked = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        masked.push_str(&rest[..open]);
        let tail = &rest[open..];
        masked.push_str("{{");
        match tail.find("}}") {
            Some(close) => {
                masked.push_str(&hide(tail[2..close].trim()));
                masked.push_str("}}");
                rest = &tail[close + 2..];
            }
            None => {
                masked.push_str(&hide(tail[2..].trim()));
                rest = "";
            }
        }
    }
    masked.push_str(rest);
    masked
}

/// Placeholders whose expansion is a whole PATH the run resolves, not a name.
///
/// `{{ .Artifact }}` is substituted before the render
/// (`stage-sign::helpers::resolve_signature_path`), and it already carries
/// `dist`. Written without padding because they are asked of text
/// `mask_placeholder_separators` has already trimmed.
const PATH_CARRYING_SPELLINGS: &[&str] = &["{{.Artifact}}", "{{Artifact}}"];

/// Shell-style variables the sign stage expands to a whole PATH after the
/// render (`stage-sign::helpers::resolve_output_paths`), in either the
/// `${name}` or the bare `$name` spelling.
const PATH_CARRYING_SHELL_VARS: &[&str] = &["artifact", "signature", "certificate"];

/// Whether `c` continues a variable or placeholder name, which is what
/// `expand_with_preserve` reads a bare `$` name to the end of.
fn continues_a_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether `text` holds `name` as a whole word.
fn names_whole_word(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(continues_a_name)
            && !text[at + name.len()..]
                .chars()
                .next()
                .is_some_and(continues_a_name)
    })
}

/// Whether `text` names `$name` or `${name}` as a WHOLE variable.
///
/// `expand_with_preserve` reads a bare `$` name to the end of its
/// alphanumeric run, so `$artifactName` and `$artifactID` are variables of
/// those names — each expanding to a NAME, not a path — and a prefix match
/// would read both as `$artifact`.
fn names_shell_var(text: &str, name: &str) -> bool {
    if text.contains(&format!("${{{name}}}")) {
        return true;
    }
    let bare = format!("${name}");
    text.match_indices(&bare).any(|(at, _)| {
        !text[at + bare.len()..]
            .chars()
            .next()
            .is_some_and(continues_a_name)
    })
}

/// The variables whose rendering the `dist` join stays correct for: one that
/// is RELATIVE, never absolute and never already under `dist`.
///
/// Both sides of a comparison are joined onto `dist`, so a rendering holding
/// a separator is still one file on both sides — `Tag` is the git tag with
/// its prefix stripped and may hold one (`release/1.0.0`), and `Version`
/// derives from it. What the join cannot survive is a rendering that is
/// ABSOLUTE or already carries `dist`, which is what every other `{{ … }}`
/// run — an `.Env.` lookup, a `Var.` reference, a function call, a filter —
/// can be.
const BOUNDED_VARIABLES: &[&str] = &[
    "ProjectName",
    "Version",
    "Binary",
    "Target",
    "Os",
    "Arch",
    "Arm",
    "Amd64",
    "Mips",
    "Tag",
];

/// Whether `masked` holds a spelling whose rendering this check cannot bound.
///
/// Asked of masked text, so a placeholder's padding is already gone. A run
/// holding anything but a bare bounded variable is unbounded, which is the
/// conservative answer: an unbounded pair is compared without the `dist`
/// join and so reads as two files, the direction a missed advisory warning
/// lies in.
fn renders_an_unbounded_path(masked: &str) -> bool {
    if PATH_CARRYING_SPELLINGS.iter().any(|s| masked.contains(s))
        || PATH_CARRYING_SHELL_VARS
            .iter()
            .any(|name| names_shell_var(masked, name))
    {
        return true;
    }
    let mut rest = masked;
    while let Some(open) = rest.find("{{") {
        let tail = &rest[open + 2..];
        let (inner, next) = match tail.find("}}") {
            Some(close) => (&tail[..close], &tail[close + 2..]),
            None => (tail, ""),
        };
        if !BOUNDED_VARIABLES.contains(&inner.trim_start_matches('.')) {
            return true;
        }
        rest = next;
    }
    false
}

/// Whether two rendered-output templates name one file.
///
/// Each spelling is placed under `dist` the way the sign stage places it
/// (`sign_outputs_are_one_file`), so `app.sig` and `dist/app.sig` are one
/// file here exactly as they are there.
///
/// A spelling whose rendering this check cannot bound is compared WITHOUT
/// that join, as a path whose `{{ … }}` runs are opaque components: the `.`
/// and `..` in the literal segments around an identical placeholder fold
/// away, and two different placeholders stay two files. Joining `dist` onto
/// a spelling that renders a path of its own would call `${artifact}.sig`
/// and `dist/${artifact}.sig` one file where the run writes `dist/app.sig`
/// and `dist/dist/app.sig`. `PATH_CARRYING_SPELLINGS`,
/// `PATH_CARRYING_SHELL_VARS` and `BOUNDED_VARIABLES` are the whole
/// derivation of "unbounded".
///
/// The `dist` asked about is the configured one. `anodizer build --dist
/// <path>` moves it at build time, so a run passing that flag places these
/// renderings against a directory this check never saw.
fn same_output_file(dist: &std::path::Path, left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let (left, right) = (
        mask_placeholder_separators(left),
        mask_placeholder_separators(right),
    );
    if renders_an_unbounded_path(&left) || renders_an_unbounded_path(&right) {
        let fold = |t: &str| anodizer_core::util::fold_dot_components(std::path::Path::new(t));
        return fold(&left) == fold(&right);
    }
    anodizer_stage_sign::sign_outputs_are_one_file(dist, &left, &right)
}

/// One sign slice an operator can write, with the provenance a diagnostic
/// needs to name the block they actually wrote.
struct SignSlice<'a> {
    /// The config path of the slice, `signs` or `workspaces.<name>.signs`.
    label: String,
    /// The `defaults.` block this slice was FILLED from, when it was. A
    /// filled slice holds an entry the operator never wrote, so a diagnostic
    /// saying `signs[0]` would point at an index that is not in their file.
    defaults_block: Option<&'static str>,
    configs: &'a Vec<anodizer_core::config::SignConfig>,
}

impl SignSlice<'_> {
    /// The block a diagnostic names for entry `idx`.
    fn block(&self, idx: usize) -> String {
        match self.defaults_block {
            Some(block) => block.to_string(),
            None => format!("{}[{idx}]", self.label),
        }
    }

    /// Whether this is a `binary_signs:` slice, which carries its own
    /// defaults for the signature template and the artifact filter.
    fn is_binary_signs(&self) -> bool {
        self.label.ends_with("binary_signs")
    }

    /// The `signature:` template an entry that sets none resolves to.
    fn signature_default(&self) -> &'static str {
        if self.is_binary_signs() {
            anodizer_core::config::SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE
        } else {
            anodizer_core::config::SignConfig::DEFAULT_SIGNATURE_TEMPLATE
        }
    }

    /// The `artifacts:` filter an entry that sets none resolves to.
    fn artifacts_default(&self) -> &'static str {
        if self.is_binary_signs() {
            anodizer_core::config::SignConfig::DEFAULT_ARTIFACTS_BINARY
        } else {
            anodizer_core::config::SignConfig::DEFAULT_ARTIFACTS
        }
    }
}

/// Every sign slice an operator can write, labelled as they wrote it.
///
/// `signs:` and `binary_signs:` hold the same `SignConfig`, resolve their
/// outputs through the same `resolve_output_paths` and write under the same
/// `dist`, so a check about how two entries' outputs collide asks both. This
/// is the one enumeration: every per-slice question — the duplicate outputs,
/// the artifact filters, the ignored `asset_name_template:` — walks it.
///
/// `docker_signs:` is deliberately outside it. Its entries carry a
/// `signature:` field of their own, but the docker sign stage never reads it
/// (`check_docker_sign_signature_templates`), so no two of them can name one
/// file.
fn sign_slices(config: &Config) -> Vec<SignSlice<'_>> {
    // `defaults.sign:` / `defaults.binary_signs:` fill an empty top-level
    // slice before any check runs, and the fold records what it filled — an
    // answer a value comparison cannot reach, since a slice the operator
    // wrote may simply repeat the default.
    let filled = |key: &'static str, block: &'static str| {
        config.filled_from_defaults.contains(key).then_some(block)
    };
    let mut slices = vec![
        SignSlice {
            label: "signs".to_string(),
            defaults_block: filled("signs", "defaults.sign"),
            configs: &config.signs,
        },
        SignSlice {
            label: "binary_signs".to_string(),
            defaults_block: filled("binary_signs", "defaults.binary_signs"),
            configs: &config.binary_signs,
        },
    ];
    for ws in config.workspaces.iter().flatten() {
        // A per-crate slice is never filled from the top-level defaults.
        slices.push(SignSlice {
            label: format!("workspaces.{}.signs", ws.name),
            defaults_block: None,
            configs: &ws.signs,
        });
        slices.push(SignSlice {
            label: format!("workspaces.{}.binary_signs", ws.name),
            defaults_block: None,
            configs: &ws.binary_signs,
        });
    }
    slices
}

/// Warn when two entries of one sign slice resolve one output FILE.
///
/// Both render the same path for any artifact both select, so the second
/// `cmd:` overwrites the first's bytes and one signature ships where two were
/// configured. The asset-name claim accepts the pair — one name over one file
/// IS one release asset — so nothing on the sign path says anything.
pub(super) fn check_sign_duplicate_outputs(config: &Config, warnings: &mut Vec<String>) {
    for slice in sign_slices(config) {
        let default = slice.signature_default();
        // The index is the one the operator wrote, so a filtered-out entry
        // does not renumber the labels of the entries after it.
        let writing: Vec<(usize, &anodizer_core::config::SignConfig)> = slice
            .configs
            .iter()
            .enumerate()
            .filter(|(_, cfg)| writes_detached_outputs(cfg))
            .collect();
        for (pos, (first, a)) in writing.iter().enumerate() {
            for (second, b) in writing.iter().skip(pos + 1) {
                if !sign_selections_overlap(a, b, slice.artifacts_default()) {
                    continue;
                }
                for (field, same) in [
                    (
                        "signature",
                        same_output_file(
                            &config.dist,
                            a.resolved_signature_template(default),
                            b.resolved_signature_template(default),
                        ),
                    ),
                    // An absent `certificate:` writes no certificate, so two
                    // absent ones are not one file.
                    (
                        "certificate",
                        match (a.certificate.as_deref(), b.certificate.as_deref()) {
                            (Some(left), Some(right)) => {
                                same_output_file(&config.dist, left, right)
                            }
                            _ => false,
                        },
                    ),
                ] {
                    if !same {
                        continue;
                    }
                    let (first_block, second_block) = (slice.block(*first), slice.block(*second));
                    warnings.push(format!(
                        "{first_block} and {second_block} resolve one {field} \
                        file for the artifacts both select — the second \
                        {field} overwrites the first, so one file ships \
                        where two were configured"
                    ));
                }
            }
        }
    }
}

/// Placeholders anodizer substitutes by exact, single-spaced literal before
/// a sign template reaches Tera (`stage-sign::helpers`).
const LITERAL_SIGN_PLACEHOLDERS: &[&str] = &["Artifact", "Signature", "Certificate"];

/// The subset `signature:` and `certificate:` substitute. Those two
/// templates are what the signature and certificate paths are DERIVED from,
/// so neither name has a value yet when they render.
const ARTIFACT_PLACEHOLDER_ONLY: &[&str] = &["Artifact"];

/// `stdin:` is handed to the template engine with nothing substituted.
const NO_SIGN_PLACEHOLDERS: &[&str] = &[];

/// Every `{{ … }}` run in `template` that names `name` — dot prefix,
/// padding and any expression around it included — as it is written.
///
/// The stage substitutes by exact literal, so the run's own text is what
/// decides whether it is one of the two spellings that works. A run that
/// names the placeholder inside an expression (`{{ Artifact | upper }}`,
/// `{{ Artifact.path }}`) is matched too: the literal replacement misses it
/// and the name is seeded in no template context, so it fails the same way a
/// mis-padded one does.
///
/// A `{# … #}` comment is skipped. Tera strips it before evaluating the
/// template, so a placeholder written inside one cannot fail the render.
fn placeholder_spellings<'a>(template: &'a str, name: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    let mut at = 0usize;
    while at < template.len() {
        let rest = &template[at..];
        let run = rest.find("{{");
        let comment = rest.find("{#");
        match (run, comment) {
            (Some(open), c) if c.is_none_or(|c| open < c) => {
                let after = &rest[open + 2..];
                let Some(close) = after.find("}}") else { break };
                if names_whole_word(after[..close].trim(), name) {
                    found.push(&rest[open..open + close + 4]);
                }
                at += open + close + 4;
            }
            (_, Some(open)) => {
                let Some(end) = rest[open + 2..].find("#}") else {
                    break;
                };
                at += open + 2 + end + 2;
            }
            _ => break,
        }
    }
    found
}

/// Warn when a sign template names a literal placeholder the field cannot
/// substitute, or writes one the stage does substitute in a spelling the
/// literal replacement misses.
///
/// The substitution is by exact literal, and the set differs per field:
/// `args:` takes all three names, `signature:` and `certificate:` take
/// `Artifact` alone, and `stdin:` takes none. Anything else reaches Tera as
/// an undefined variable and hard-errors the sign stage — `{{.Artifact}}`,
/// `{{ .Artifact}}` and `{{ Artifact | upper }}` alike, since only
/// `{{ .Artifact }}` and `{{ Artifact }}` are replaced.
pub(super) fn check_unpadded_sign_placeholders(config: &Config, warnings: &mut Vec<String>) {
    let mut warn =
        |block: &str, field: &str, substituted: &[&str], shell_expanded: bool, template: &str| {
            for name in LITERAL_SIGN_PLACEHOLDERS {
                for spelling in placeholder_spellings(template, name) {
                    if !substituted.contains(name) {
                        let shell = name.to_ascii_lowercase();
                        // A field is what its own path is DERIVED from, so
                        // the shell variable of the same name holds this
                        // template's own unexpanded text while it resolves:
                        // `${signature}` inside `signature:` expands to a
                        // file name carrying a literal `$`.
                        let remedy = if shell == field {
                            format!(
                                "; the {field} path is what this template \
                                renders, so `${{{shell}}}` has no value here \
                                either — remove the reference"
                            )
                        } else if shell_expanded {
                            // Every other shell-expanded field resolves the
                            // `${…}` spelling after the render, so it needs
                            // no template variable at all.
                            format!(
                                "; write `${{{shell}}}`, which the sign stage \
                                expands after the render"
                            )
                        } else {
                            String::new()
                        };
                        warnings.push(format!(
                            "{block}.{field} names `{spelling}`, which anodizer \
                            does not substitute in {field}: — it reaches the \
                            template engine as an undefined variable and fails \
                            the sign stage{remedy}"
                        ));
                    } else if spelling != format!("{{{{ .{name} }}}}")
                        && spelling != format!("{{{{ {name} }}}}")
                    {
                        warnings.push(format!(
                            "{block}.{field} names `{spelling}`, which anodizer \
                            substitutes only as the literal `{{{{ .{name} }}}}` \
                            or `{{{{ {name} }}}}` — every other spelling \
                            reaches the template engine as an undefined \
                            variable and fails the sign stage"
                        ));
                    }
                }
            }
        };
    for slice in sign_slices(config) {
        for (idx, cfg) in slice.configs.iter().enumerate() {
            let block = slice.block(idx);
            for (field, substituted, template) in [
                (
                    "signature",
                    ARTIFACT_PLACEHOLDER_ONLY,
                    cfg.signature.as_deref(),
                ),
                (
                    "certificate",
                    ARTIFACT_PLACEHOLDER_ONLY,
                    cfg.certificate.as_deref(),
                ),
                ("stdin", NO_SIGN_PLACEHOLDERS, cfg.stdin.as_deref()),
            ] {
                if let Some(template) = template {
                    warn(&block, field, substituted, true, template);
                }
            }
            for arg in cfg.args.iter().flatten() {
                warn(&block, "args", LITERAL_SIGN_PLACEHOLDERS, true, arg);
            }
        }
    }
    for (idx, cfg) in config.docker_signs.iter().flatten().enumerate() {
        let block = docker_sign_block(config, idx);
        // A docker sign's argv is substituted the same three ways, but its
        // `stdin:` is rendered raw with no shell expansion behind it, so
        // there is no spelling of a placeholder that works there.
        for arg in cfg.args.iter().flatten() {
            warn(&block, "args", LITERAL_SIGN_PLACEHOLDERS, false, arg);
        }
        if let Some(stdin) = cfg.stdin.as_deref() {
            warn(&block, "stdin", NO_SIGN_PLACEHOLDERS, false, stdin);
        }
    }
}

/// The block a diagnostic names for `docker_signs[idx]`, `defaults.` block
/// included when the fold filled the slice.
fn docker_sign_block(config: &Config, idx: usize) -> String {
    match config.filled_from_defaults.contains("docker_signs") {
        true => "defaults.docker_signs".to_string(),
        false => format!("docker_signs[{idx}]"),
    }
}

/// Warn that a `docker_signs:` entry's `signature:` names nothing.
///
/// A container signature is stored in the registry beside the image rather
/// than written to disk, so the docker sign stage synthesizes the
/// `<image>@<digest>.sig` name its argv substitutes and reads this template
/// nowhere. An operator who sets it gets the synthesized name with no error
/// at all — and no two entries can resolve one output file, which is why the
/// duplicate-output question is asked of `signs:` and `binary_signs:` only.
pub(super) fn check_docker_sign_signature_templates(config: &Config, warnings: &mut Vec<String>) {
    for (idx, cfg) in config.docker_signs.iter().flatten().enumerate() {
        if cfg.signature.is_some() {
            warnings.push(format!(
                "{}.signature is set but a docker signature is stored in the \
                registry rather than written to a file (it will be ignored)",
                docker_sign_block(config, idx)
            ));
        }
    }
}
