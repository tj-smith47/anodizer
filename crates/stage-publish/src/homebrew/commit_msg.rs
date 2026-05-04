use anodizer_core::template::{self, TemplateVars};

/// Render a commit message from an optional Tera template string.
///
/// The template receives `ProjectName` (= name) and `Tag`/`Version` variables
/// matching GoReleaser's template context.  When `template` is `None` the
/// GoReleaser default for the publisher `kind` is used.
pub(crate) fn render_commit_msg(
    template: Option<&str>,
    name: &str,
    version: &str,
    kind: &str,
) -> String {
    render_commit_msg_with_prev(template, name, version, "", kind)
}

/// Render a commit message with PreviousTag support (for Nix publisher).
pub(crate) fn render_commit_msg_with_prev(
    template: Option<&str>,
    name: &str,
    version: &str,
    previous_tag: &str,
    kind: &str,
) -> String {
    // GoReleaser default commit messages per publisher type:
    //   brew formula: "Brew formula update for {{ .ProjectName }} version {{ .Tag }}"
    //   brew cask:    "Brew cask update for {{ .ProjectName }} version {{ .Tag }}"
    //   krew:         "Krew manifest update for {{ .ProjectName }} version {{ .Tag }}"
    //   aur:          "Update to {{ .Tag }}"
    //   nix:          "{{ .ProjectName }}: {{ .PreviousTag }} -> {{ .Tag }}"
    //   winget:       "{{ .PackageIdentifier }} version {{ .Version }}"
    let default_tmpl = match kind {
        "formula" => "Brew formula update for {{ ProjectName }} version {{ Tag }}".to_string(),
        "cask" => "Brew cask update for {{ ProjectName }} version {{ Tag }}".to_string(),
        "plugin" => "Krew manifest update for {{ ProjectName }} version {{ Tag }}".to_string(),
        "package" => "Update to {{ Tag }}".to_string(),
        "nix" => "{{ ProjectName }}: {{ PreviousTag }} -> {{ Tag }}".to_string(),
        _ => format!("Update {{{{ ProjectName }}}} {} to {{{{ Tag }}}}", kind),
    };
    let tmpl = template.unwrap_or(&default_tmpl);

    let mut vars = TemplateVars::new();
    vars.set("ProjectName", name);
    vars.set("Tag", version);
    vars.set("Version", version);
    vars.set("PreviousTag", previous_tag);
    vars.set("name", name);
    vars.set("version", version);
    template::render(tmpl, &vars)
        .unwrap_or_else(|_| format!("{} update for {} version {}", kind, name, version))
}
