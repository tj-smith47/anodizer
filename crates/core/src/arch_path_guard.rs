//! Guard that converts a silent per-architecture output clobber into an
//! immediate, actionable error.
//!
//! Every OS-installer stage (`app_bundles`, `dmgs`, `pkgs`, `msis`, `nsis`)
//! loops once per build target and writes its artifact to a path derived
//! from the user's `name` template. If that template omits `{{ .Arch }}`,
//! two architectures render the *same* output path and the second silently
//! overwrites the first — and any downstream `use:` consumer (e.g.
//! `dmgs.use: appbundle`) then wraps the lone survivor twice, producing
//! per-arch artifacts with wrong-arch payloads. The stage default templates
//! all carry `{{ .Arch }}`, so this only fires on a bad override; it makes
//! the mistake impossible to ship silently.
//!
//! Construct one [`ArchPathGuard`] per guarded scope — per crate for the
//! installer stages, per stage run for archives — and call
//! [`ArchPathGuard::check`] with one [`Claim`] per rendered output path.
//! The first duplicate returns an error naming the offending template and
//! crate, and a remedy naming the ONE template variable that separates the
//! two claims, chosen by how they differ (first match wins):
//!
//! | prior vs new claim | remedy variable |
//! |---|---|
//! | different crate | `{{ .CrateName }}` |
//! | same crate, target and amd64 variant | `{{ .Binary }}` |
//! | same crate and target, different amd64 variant | `{{ .Amd64 }}` |
//! | same crate, OS and architecture, different triple (gnu vs musl) | `{{ .Target }}` |
//! | same architecture, different OS | `{{ .Os }}` |
//! | otherwise | `{{ .Arch }}` |
//!
//! A variable is only advised when the claim's [`Claim::exposed`] set — the
//! names the stage's naming context defines with a non-empty value — carries
//! it; a stage whose templates cannot see the variable is told to give the
//! colliding entries distinct templates instead. The scope must span more
//! than one config, since a stage's config is a `Vec`: two configs rendering
//! the same path have to collide loudly instead of the second silently
//! clobbering the first.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::target::map_target;

/// One output path a stage is about to write, with everything the
/// collision diagnostic needs.
#[derive(Debug, Clone, Copy)]
pub struct Claim<'a> {
    /// The output path being claimed.
    pub path: &'a Path,
    /// The stage's config key (`"dmgs"`), leading the message.
    pub stage: &'a str,
    /// Singular user-facing noun for the output, interpolated as-is
    /// (`"image"` reads `rendered the same image '<name>' more than once`).
    pub artifact: &'a str,
    /// The config key holding the name template (`"name"`,
    /// `"name_template"`, `"file_name_template"`), so the remedy names the
    /// field the user must edit.
    pub template_key: &'a str,
    /// The user template that rendered the name; `None` when the stage's
    /// conventional default filename did (distro packagers whose default
    /// keeps the arch field bare), which the message says in those words.
    pub name_template: Option<&'a str>,
    /// The rendered output name.
    pub rendered: &'a str,
    /// The crate being built.
    pub crate_name: &'a str,
    /// The build target (triple) the output belongs to; `None` for a host
    /// build with no explicit target.
    pub target: Option<&'a str>,
    /// The amd64 micro-architecture level (`v3`); `None` for the baseline
    /// or a non-amd64 target.
    pub amd64_variant: Option<&'a str>,
    /// Names of the template variables the naming context defines with a
    /// non-empty value (see [`TemplateVars::defined_names`]); the remedy
    /// advises only variables in this set.
    ///
    /// [`TemplateVars::defined_names`]: crate::template::TemplateVars::defined_names
    pub exposed: &'a BTreeSet<String>,
}

/// The crate, build target and amd64 variant that first claimed an output
/// path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Prior {
    crate_name: String,
    target: Option<String>,
    amd64_variant: Option<String>,
}

/// Tracks the output paths one guarded scope has produced across every
/// per-architecture artifact loop, erroring on the first collision.
#[derive(Debug, Default)]
pub struct ArchPathGuard {
    seen: HashMap<PathBuf, Prior>,
}

impl ArchPathGuard {
    /// A fresh guard with no recorded paths. One per guarded scope, shared
    /// across every config that scope covers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the claim's path for this scope; error if a previous claim
    /// already produced it.
    pub fn check(&mut self, claim: Claim<'_>) -> anyhow::Result<()> {
        let prior = Prior {
            crate_name: claim.crate_name.to_string(),
            target: claim.target.map(str::to_string),
            amd64_variant: claim.amd64_variant.map(str::to_string),
        };
        let Some(prev) = self.seen.get(claim.path) else {
            self.seen.insert(claim.path.to_path_buf(), prior);
            return Ok(());
        };
        anyhow::bail!("{}", collision_message(&claim, prev));
    }
}

/// `{{ .Name }}` when the naming context exposes `name`.
fn var(claim: &Claim<'_>, name: &str) -> Option<String> {
    claim
        .exposed
        .contains(name)
        .then(|| format!("{{{{ .{name} }}}}"))
}

/// The full diagnostic for a path claimed twice within one scope.
fn collision_message(claim: &Claim<'_>, prev: &Prior) -> String {
    let artifact = claim.artifact;
    let key = claim.template_key;
    let source = match claim.name_template {
        Some(tmpl) => format!("name template '{tmpl}'"),
        None => "the conventional default filename".to_string(),
    };
    // A user template is edited; a conventional default is replaced by one.
    let add = |v: &str, example: &str| match claim.name_template {
        Some(_) => format!("add '{v}' to the `{key}` (e.g. \"{example}\")"),
        None => format!("set a `{key}` carrying '{v}' (e.g. \"{example}\")"),
    };
    let distinct = |what: &str| format!("give {what} a distinct `{key}`");

    let same_crate = prev.crate_name == claim.crate_name;
    let same_target = same_crate && prev.target.as_deref() == claim.target;
    let (reason, remedy) = if !same_crate {
        let reason = format!(
            "The collision is between crates '{}' and '{}', so no target variable can \
             separate them",
            prev.crate_name, claim.crate_name
        );
        let remedy = match var(claim, "CrateName") {
            Some(v) => add(&v, &format!("{v}_{{{{ .Os }}}}_{{{{ .Arch }}}}")),
            None => distinct("each crate's entry"),
        };
        (reason, remedy)
    } else if same_target && prev.amd64_variant.as_deref() == claim.amd64_variant {
        let reason = format!(
            "Both come from the same build target '{}', so no architecture variable can \
             separate them",
            claim.target.unwrap_or("host")
        );
        let remedy = match var(claim, "Binary") {
            Some(v) => format!(
                "{} when the entry ships more than one binary, or {}",
                add(&v, &format!("{v}_{{{{ .Os }}}}_{{{{ .Arch }}}}")),
                distinct("each config entry")
            ),
            None => distinct("each config entry"),
        };
        (reason, remedy)
    } else if same_target {
        let reason = "The collision is between two amd64 micro-architecture variants of one \
                      target (e.g. a baseline build and one tuned with -Ctarget-cpu=x86-64-v3)"
            .to_string();
        let remedy = match var(claim, "Amd64") {
            Some(v) => add(&v, &format!("{{{{ .ProjectName }}}}_{{{{ .Arch }}}}{v}")),
            None => distinct("each variant's entry"),
        };
        (reason, remedy)
    } else {
        let (prev_os, prev_arch) = prev.target.as_deref().map(map_target).unwrap_or_default();
        let (os, arch) = claim.target.map(map_target).unwrap_or_default();
        if prev_os == os && prev_arch == arch {
            let reason = format!(
                "Build targets '{}' and '{}' share OS '{os}' and architecture '{arch}', so \
                 neither `{{{{ .Os }}}}` nor `{{{{ .Arch }}}}` can separate them",
                prev.target.as_deref().unwrap_or("host"),
                claim.target.unwrap_or("host")
            );
            let remedy = match var(claim, "Target") {
                Some(v) => add(&v, &format!("{{{{ .ProjectName }}}}_{v}")),
                None => distinct("each target's entry"),
            };
            (reason, remedy)
        } else {
            let reason = format!(
                "The collision is between build targets '{}' and '{}'",
                prev.target.as_deref().unwrap_or("host"),
                claim.target.unwrap_or("host")
            );
            let name = if prev_arch == arch && var(claim, "Os").is_some() {
                "Os"
            } else {
                "Arch"
            };
            let remedy = match var(claim, name) {
                Some(v) => add(&v, &format!("{{{{ .ProjectName }}}}_{v}")),
                None => distinct("each target's entry"),
            };
            (reason, remedy)
        }
    };
    format!(
        "{stage}: {source} rendered the same {artifact} '{rendered}' more than once for crate \
         '{crate_name}', so one {artifact} would silently overwrite another. {reason}: \
         {remedy} so each {artifact} gets a distinct path.",
        stage = claim.stage,
        rendered = claim.rendered,
        crate_name = claim.crate_name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exposed(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    /// A claim on `path` by `crate_name` / `target` / `variant`, rendered
    /// from `template` with the archive stage's full naming context.
    fn claim<'a>(
        path: &'a Path,
        template: &'a str,
        crate_name: &'a str,
        target: Option<&'a str>,
        variant: Option<&'a str>,
        exposed: &'a BTreeSet<String>,
    ) -> Claim<'a> {
        Claim {
            path,
            stage: "archives",
            artifact: "binary",
            template_key: "name_template",
            name_template: Some(template),
            rendered: path.file_name().unwrap().to_str().unwrap(),
            crate_name,
            target,
            amd64_variant: variant,
            exposed,
        }
    }

    const ALL: [&str; 6] = ["CrateName", "Binary", "Amd64", "Target", "Os", "Arch"];

    #[test]
    fn distinct_paths_pass() {
        let all = exposed(&ALL);
        let mut guard = ArchPathGuard::new();
        let a = Path::new("dist/macos/app_amd64.app");
        let b = Path::new("dist/macos/app_arm64.app");
        guard
            .check(claim(
                a,
                "{{ .ProjectName }}_{{ .Arch }}",
                "app",
                Some("x86_64-apple-darwin"),
                None,
                &all,
            ))
            .expect("first path must pass");
        guard
            .check(claim(
                b,
                "{{ .ProjectName }}_{{ .Arch }}",
                "app",
                Some("aarch64-apple-darwin"),
                None,
                &all,
            ))
            .expect("distinct second path must pass");
    }

    #[test]
    fn duplicate_path_bails_with_actionable_message() {
        let all = exposed(&ALL);
        let mut guard = ArchPathGuard::new();
        let path = Path::new("dist/macos/app.app");
        let mut c = claim(
            path,
            "{{ .ProjectName }}",
            "app",
            Some("x86_64-apple-darwin"),
            None,
            &all,
        );
        c.stage = "app_bundles";
        c.artifact = "bundle";
        c.template_key = "name";
        guard.check(c).expect("first path must pass");
        c.target = Some("aarch64-apple-darwin");
        let err = guard.check(c).unwrap_err().to_string();

        assert!(err.contains("app_bundles:"), "{err}");
        assert!(err.contains("crate 'app'"), "{err}");
        assert!(err.contains("add '{{ .Arch }}' to the `name`"), "{err}");
        assert!(!err.contains("{{ .Binary }}"), "{err}");
        assert!(!err.contains("{{ .Amd64 }}"), "{err}");
        assert!(err.contains("bundle gets a distinct path"), "{err}");
    }

    #[test]
    fn cross_os_same_arch_collision_names_the_os_var() {
        // amd64 linux vs amd64 darwin: `.Arch` renders the same for both, so
        // advising it would reproduce the collision.
        let all = exposed(&ALL);
        let mut guard = ArchPathGuard::new();
        let path = Path::new("dist/app_amd64");
        let mut c = claim(
            path,
            "{{ .ProjectName }}_{{ .Arch }}",
            "app",
            Some("x86_64-unknown-linux-gnu"),
            None,
            &all,
        );
        guard.check(c).unwrap();
        c.target = Some("x86_64-apple-darwin");
        let err = guard.check(c).unwrap_err().to_string();
        assert!(err.contains("add '{{ .Os }}'"), "{err}");
        assert!(!err.contains("'{{ .Arch }}'"), "{err}");
    }

    #[test]
    fn same_target_variant_collision_names_the_amd64_var() {
        // A baseline and a v3-tuned build of ONE triple share crate and
        // target; only `.Amd64` separates them.
        let all = exposed(&ALL);
        let mut guard = ArchPathGuard::new();
        let path = Path::new("dist/app.AppImage");
        let target = Some("x86_64-unknown-linux-gnu");
        let mut c = claim(path, "{{ .ProjectName }}", "app", target, None, &all);
        c.stage = "appimage";
        c.artifact = "image";
        guard.check(c).expect("first path must pass");
        c.amd64_variant = Some("v3");
        let err = guard.check(c).unwrap_err().to_string();

        assert!(err.contains("add '{{ .Amd64 }}'"), "{err}");
        assert!(!err.contains("{{ .Binary }}"), "{err}");
        assert!(!err.contains("add '{{ .Arch }}'"), "{err}");
    }

    #[test]
    fn same_crate_same_target_collision_names_the_binary_var() {
        // Two binaries of one crate on one target through a template with no
        // `.Binary` render one path; `.Arch` cannot separate them, so the
        // remedy must name `{{ .Binary }}` and not the arch variables.
        let all = exposed(&ALL);
        let mut guard = ArchPathGuard::new();
        let path = Path::new("dist/proj_linux");
        let target = Some("x86_64-unknown-linux-gnu");
        let c = claim(
            path,
            "{{ .ProjectName }}_{{ .Os }}",
            "app",
            target,
            None,
            &all,
        );
        guard.check(c).expect("first path must pass");
        let err = guard.check(c).unwrap_err().to_string();

        assert!(err.contains("archives:"), "{err}");
        assert!(err.contains("crate 'app'"), "{err}");
        assert!(
            err.contains("same build target 'x86_64-unknown-linux-gnu'"),
            "{err}"
        );
        assert!(
            err.contains("add '{{ .Binary }}' to the `name_template`"),
            "{err}"
        );
        assert!(
            !err.contains("'{{ .Arch }}'"),
            "must not advise .Arch: {err}"
        );
        assert!(
            !err.contains("{{ .Amd64 }}"),
            "must not advise .Amd64: {err}"
        );
    }

    #[test]
    fn same_target_collision_without_binary_var_advises_distinct_templates() {
        // The installer stages never define `.Binary`; advising it there
        // would render an empty string. The remedy must fall back to
        // distinct per-entry templates and never mention the variable.
        let installer = exposed(&["Os", "Arch", "Target", "Amd64"]);
        let mut guard = ArchPathGuard::new();
        let path = Path::new("dist/macos/app.dmg");
        let mut c = claim(
            path,
            "{{ .ProjectName }}",
            "app",
            Some("x86_64-apple-darwin"),
            None,
            &installer,
        );
        c.stage = "dmgs";
        c.artifact = "image";
        c.template_key = "name";
        guard.check(c).unwrap();
        let err = guard.check(c).unwrap_err().to_string();
        assert!(!err.contains(".Binary"), "{err}");
        assert!(
            err.contains("give each config entry a distinct `name`"),
            "{err}"
        );
    }

    #[test]
    fn cross_crate_collision_names_the_crate_var() {
        // Two crates render one name from `shared_{{ .Os }}`; they share the
        // target, so `.Arch` / `.Binary` would reproduce the collision and
        // only the crate variable separates them.
        let all = exposed(&ALL);
        let mut guard = ArchPathGuard::new();
        let path = Path::new("dist/shared_linux");
        let target = Some("x86_64-unknown-linux-gnu");
        let mut c = claim(path, "shared_{{ .Os }}", "myapp", target, None, &all);
        guard.check(c).unwrap();
        c.crate_name = "mytool";
        let err = guard.check(c).unwrap_err().to_string();
        assert!(err.contains("between crates 'myapp' and 'mytool'"), "{err}");
        assert!(err.contains("add '{{ .CrateName }}'"), "{err}");
        assert!(!err.contains("'{{ .Arch }}'"), "{err}");
        assert!(!err.contains("'{{ .Binary }}'"), "{err}");
    }

    #[test]
    fn cross_crate_collision_without_crate_var_advises_distinct_templates() {
        let installer = exposed(&["Os", "Arch", "Target"]);
        let mut guard = ArchPathGuard::new();
        let path = Path::new("dist/macos/shared.dmg");
        let mut c = claim(
            path,
            "shared",
            "myapp",
            Some("x86_64-apple-darwin"),
            None,
            &installer,
        );
        c.template_key = "name";
        guard.check(c).unwrap();
        c.crate_name = "mytool";
        let err = guard.check(c).unwrap_err().to_string();
        assert!(!err.contains(".CrateName"), "{err}");
        assert!(
            err.contains("give each crate's entry a distinct `name`"),
            "{err}"
        );
    }

    #[test]
    fn same_os_arch_different_triple_collision_names_the_target_var() {
        // gnu vs musl: identical `.Os` / `.Arch`, so a template already
        // carrying both still collides and only the triple separates them.
        let all = exposed(&ALL);
        let mut guard = ArchPathGuard::new();
        let path = Path::new("dist/myapp_linux_amd64");
        let mut c = claim(
            path,
            "{{ .Binary }}_{{ .Os }}_{{ .Arch }}",
            "myapp",
            Some("x86_64-unknown-linux-gnu"),
            None,
            &all,
        );
        guard.check(c).unwrap();
        c.target = Some("x86_64-unknown-linux-musl");
        let err = guard.check(c).unwrap_err().to_string();
        assert!(
            err.contains("share OS 'linux' and architecture 'amd64'"),
            "{err}"
        );
        assert!(err.contains("add '{{ .Target }}'"), "{err}");
        assert!(!err.contains("add '{{ .Arch }}'"), "{err}");
    }

    #[test]
    fn conventional_default_collision_bails_without_fake_template() {
        // The conventional-default path must NOT echo a fabricated template and
        // must advise `{{ .Amd64 }}` (the conventional default already carries
        // `{{ .Arch }}`, so advising `{{ .Arch }}` would be useless).
        let nfpm = exposed(&["Os", "Arch", "Target", "Amd64"]);
        let mut guard = ArchPathGuard::new();
        let path = Path::new("dist/linux/myapp_1.0.0_amd64.deb");
        let mut c = Claim {
            path,
            stage: "nfpms",
            artifact: "package",
            template_key: "file_name_template",
            name_template: None,
            rendered: "myapp_1.0.0_amd64.deb",
            crate_name: "myapp",
            target: Some("x86_64-unknown-linux-gnu"),
            amd64_variant: None,
            exposed: &nfpm,
        };
        guard.check(c).expect("first path must pass");
        c.amd64_variant = Some("v3");
        let err = guard.check(c).unwrap_err().to_string();

        assert!(err.contains("nfpms:"), "{err}");
        assert!(err.contains("crate 'myapp'"), "{err}");
        assert!(err.contains("conventional default filename"), "{err}");
        assert!(
            err.contains("set a `file_name_template` carrying '{{ .Amd64 }}'"),
            "{err}"
        );
        assert!(
            !err.contains("name template '"),
            "must not echo a fake template: {err}"
        );
        assert!(err.contains("package gets a distinct path"), "{err}");
    }

    #[test]
    fn separate_scopes_do_not_share_state() {
        // Two crates (or two config entries) each render the same leaf path;
        // a per-scope guard must NOT treat the second scope's first write as
        // a collision.
        let all = exposed(&ALL);
        let path = Path::new("dist/macos/app.app");
        let mut first = ArchPathGuard::new();
        first
            .check(claim(path, "{{ .ProjectName }}", "a", None, None, &all))
            .expect("scope one first write");
        let mut second = ArchPathGuard::new();
        second
            .check(claim(path, "{{ .ProjectName }}", "b", None, None, &all))
            .expect("scope two first write must pass");
    }
}
