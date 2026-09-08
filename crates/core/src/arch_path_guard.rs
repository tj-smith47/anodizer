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
//! [`ArchPathGuard::check`] with each rendered output path and the build
//! target it belongs to. The first duplicate returns an error naming the
//! offending template and crate, and a remedy that fits how the two claims
//! differ: a collision across build targets wants `{{ .Arch }}`, one between
//! two amd64 micro-architecture variants of a target wants `{{ .Amd64 }}`,
//! and one between two outputs of the same crate, target and variant (two
//! binaries through one `binary`-format template) wants `{{ .Binary }}` —
//! no architecture variable can tell those apart. The scope must span more
//! than one config, since a stage's config is a `Vec`: two configs
//! rendering the same path have to collide loudly instead of the second
//! silently clobbering the first.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The crate, build target and amd64 variant that first claimed an output
/// path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Claim {
    crate_name: String,
    target: Option<String>,
    amd64_variant: Option<String>,
}

/// Tracks the output paths one guarded scope has produced across every
/// per-architecture artifact loop, erroring on the first collision.
#[derive(Debug, Default)]
pub struct ArchPathGuard {
    seen: HashMap<PathBuf, Claim>,
}

impl ArchPathGuard {
    /// A fresh guard with no recorded paths. One per guarded scope, shared
    /// across every config that scope covers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `path` as claimed by `crate_name` on `target` / `amd64_variant`;
    /// the previous claim when the path was already taken.
    fn claim(
        &mut self,
        path: &Path,
        crate_name: &str,
        target: Option<&str>,
        amd64_variant: Option<&str>,
    ) -> Option<Claim> {
        let claim = Claim {
            crate_name: crate_name.to_string(),
            target: target.map(str::to_string),
            amd64_variant: amd64_variant.map(str::to_string),
        };
        match self.seen.get(path) {
            Some(prev) => Some(prev.clone()),
            None => {
                self.seen.insert(path.to_path_buf(), claim);
                None
            }
        }
    }

    /// Record `path` for this scope; error if a previous call already
    /// produced it.
    ///
    /// `stage` is the config key (`"dmgs"`), `artifact` the singular
    /// user-facing noun the message interpolates as-is (`"image"` reads
    /// `rendered the same image '<name>' more than once`), `name_template` the
    /// offending template, `rendered` the rendered output name, `crate_name`
    /// the crate being built, `target` the build target (triple) the output
    /// belongs to (`None` for a host build with no explicit target) and
    /// `amd64_variant` its micro-architecture level (`v3`; `None` for the
    /// baseline or a non-amd64 target).
    ///
    /// The remedy names the one template variable that separates the two
    /// claims: `{{ .Arch }}` across build targets, `{{ .Amd64 }}` across
    /// variants of one target, `{{ .Binary }}` when both come from the same
    /// crate, target and variant (two binaries rendered through one
    /// template).
    #[allow(clippy::too_many_arguments)]
    pub fn check(
        &mut self,
        path: &Path,
        stage: &str,
        artifact: &str,
        name_template: &str,
        rendered: &str,
        crate_name: &str,
        target: Option<&str>,
        amd64_variant: Option<&str>,
    ) -> anyhow::Result<()> {
        let Some(prev) = self.claim(path, crate_name, target, amd64_variant) else {
            return Ok(());
        };
        let same_target = prev.crate_name == crate_name && prev.target.as_deref() == target;
        if same_target && prev.amd64_variant.as_deref() == amd64_variant {
            anyhow::bail!(
                "{stage}: name template '{name_template}' rendered the same {artifact} \
                 '{rendered}' more than once for crate '{crate_name}' on build target \
                 '{}', so one {artifact} would silently overwrite another. Both come \
                 from the same build target, so no architecture variable can separate \
                 them: add '{{{{ .Binary }}}}' to the `name` \
                 (e.g. \"{{{{ .Binary }}}}_{{{{ .Os }}}}_{{{{ .Arch }}}}\") when the entry \
                 ships more than one binary, or give each config entry a distinct \
                 `name`.",
                target.unwrap_or("host")
            );
        }
        if same_target {
            anyhow::bail!(
                "{stage}: name template '{name_template}' rendered the same {artifact} \
                 '{rendered}' more than once for crate '{crate_name}', so one build \
                 target would silently overwrite another. The collision is between \
                 two amd64 micro-architecture variants of one target (e.g. a baseline \
                 build and one tuned with -Ctarget-cpu=x86-64-v3): add '{{{{ .Amd64 }}}}' \
                 to the `name` (e.g. \"{{{{ .ProjectName }}}}_{{{{ .Arch }}}}{{{{ .Amd64 }}}}\") \
                 so each variant's {artifact} gets a distinct path."
            );
        }
        anyhow::bail!(
            "{stage}: name template '{name_template}' rendered the same {artifact} \
             '{rendered}' more than once for crate '{crate_name}', so one build target \
             would silently overwrite another. Add '{{{{ .Arch }}}}' to the `name` \
             (e.g. \"{{{{ .ProjectName }}}}_{{{{ .Arch }}}}\") so each build target's \
             {artifact} gets a distinct path."
        );
    }

    /// Record `path` for this scope when the name comes from a stage's
    /// *conventional default* filename rather than a user template; error if a
    /// previous call already produced it.
    ///
    /// For distro-conventional packagers (deb/rpm/apk), the default filename's
    /// arch field must stay bare (`amd64`, never `amd64v3`), so two amd64
    /// micro-architecture variants of one triple render the same path under the
    /// default. The message names "the conventional default filename" (no fake
    /// template echo) and advises a `file_name_template` carrying
    /// `{{ .Amd64 }}` — the only field that disambiguates same-arch variants.
    #[allow(clippy::too_many_arguments)]
    pub fn check_conventional(
        &mut self,
        path: &Path,
        stage: &str,
        artifact: &str,
        rendered: &str,
        crate_name: &str,
        target: Option<&str>,
        amd64_variant: Option<&str>,
    ) -> anyhow::Result<()> {
        if self
            .claim(path, crate_name, target, amd64_variant)
            .is_none()
        {
            return Ok(());
        }
        anyhow::bail!(
            "{stage}: the conventional default filename rendered the same {artifact} \
             '{rendered}' more than once for crate '{crate_name}', so one build target \
             would silently overwrite another. This happens when two same-arch \
             micro-architecture variants (e.g. a baseline amd64 build and one tuned \
             with -Ctarget-cpu=x86-64-v3) share the distro-conventional arch field. \
             Set a `file_name_template` carrying '{{{{ .Amd64 }}}}' \
             (e.g. \"{{{{ .PackageName }}}}_{{{{ .Version }}}}_{{{{ .Arch }}}}{{{{ .Amd64 }}}}\") \
             so each variant's {artifact} gets a distinct path."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_paths_pass() {
        let mut guard = ArchPathGuard::new();
        guard
            .check(
                Path::new("dist/macos/app_amd64.app"),
                "app_bundles",
                "bundle",
                "{{ .ProjectName }}_{{ .Arch }}",
                "app_amd64.app",
                "app",
                Some("x86_64-apple-darwin"),
                None,
            )
            .expect("first path must pass");
        guard
            .check(
                Path::new("dist/macos/app_arm64.app"),
                "app_bundles",
                "bundle",
                "{{ .ProjectName }}_{{ .Arch }}",
                "app_arm64.app",
                "app",
                Some("aarch64-apple-darwin"),
                None,
            )
            .expect("distinct second path must pass");
    }

    #[test]
    fn duplicate_path_bails_with_actionable_message() {
        let mut guard = ArchPathGuard::new();
        guard
            .check(
                Path::new("dist/macos/app.app"),
                "app_bundles",
                "bundle",
                "{{ .ProjectName }}",
                "app.app",
                "app",
                Some("x86_64-apple-darwin"),
                None,
            )
            .expect("first path must pass");

        let err = guard
            .check(
                Path::new("dist/macos/app.app"),
                "app_bundles",
                "bundle",
                "{{ .ProjectName }}",
                "app.app",
                "app",
                Some("aarch64-apple-darwin"),
                None,
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("app_bundles:"), "{err}");
        assert!(err.contains("crate 'app'"), "{err}");
        assert!(err.contains("{{ .Arch }}"), "{err}");
        assert!(!err.contains("{{ .Binary }}"), "{err}");
        assert!(!err.contains("{{ .Amd64 }}"), "{err}");
        assert!(err.contains("bundle gets a distinct path"), "{err}");
    }

    #[test]
    fn same_target_variant_collision_names_the_amd64_var() {
        // A baseline and a v3-tuned build of ONE triple share crate and
        // target; only `.Amd64` separates them.
        let mut guard = ArchPathGuard::new();
        let target = Some("x86_64-unknown-linux-gnu");
        guard
            .check(
                Path::new("dist/app.AppImage"),
                "appimage",
                "image",
                "{{ .ProjectName }}",
                "app.AppImage",
                "app",
                target,
                None,
            )
            .expect("first path must pass");

        let err = guard
            .check(
                Path::new("dist/app.AppImage"),
                "appimage",
                "image",
                "{{ .ProjectName }}",
                "app.AppImage",
                "app",
                target,
                Some("v3"),
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("{{ .Amd64 }}"), "{err}");
        assert!(!err.contains("{{ .Binary }}"), "{err}");
        assert!(!err.contains("Add '{{ .Arch }}'"), "{err}");
    }

    #[test]
    fn same_crate_same_target_collision_names_the_binary_var() {
        // Two binaries of one crate on one target through a template with no
        // `.Binary` render one path; `.Arch` cannot separate them, so the
        // remedy must name `{{ .Binary }}` and not the arch variables.
        let mut guard = ArchPathGuard::new();
        let target = Some("x86_64-unknown-linux-gnu");
        guard
            .check(
                Path::new("dist/proj_linux"),
                "archives",
                "binary",
                "{{ .ProjectName }}_{{ .Os }}",
                "proj_linux",
                "app",
                target,
                None,
            )
            .expect("first path must pass");

        let err = guard
            .check(
                Path::new("dist/proj_linux"),
                "archives",
                "binary",
                "{{ .ProjectName }}_{{ .Os }}",
                "proj_linux",
                "app",
                target,
                None,
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("archives:"), "{err}");
        assert!(err.contains("crate 'app'"), "{err}");
        assert!(
            err.contains("on build target 'x86_64-unknown-linux-gnu'"),
            "{err}"
        );
        assert!(err.contains("{{ .Binary }}"), "{err}");
        assert!(
            !err.contains("{{ .Arch }}'"),
            "must not advise .Arch: {err}"
        );
        assert!(
            !err.contains("{{ .Amd64 }}"),
            "must not advise .Amd64: {err}"
        );
    }

    #[test]
    fn conventional_default_collision_bails_without_fake_template() {
        // The conventional-default path must NOT echo a fabricated template and
        // must advise `{{ .Amd64 }}` (the conventional default already carries
        // `{{ .Arch }}`, so advising `{{ .Arch }}` would be useless).
        let mut guard = ArchPathGuard::new();
        guard
            .check_conventional(
                Path::new("dist/linux/myapp_1.0.0_amd64.deb"),
                "nfpms",
                "package",
                "myapp_1.0.0_amd64.deb",
                "myapp",
                Some("x86_64-unknown-linux-gnu"),
                None,
            )
            .expect("first path must pass");

        let err = guard
            .check_conventional(
                Path::new("dist/linux/myapp_1.0.0_amd64.deb"),
                "nfpms",
                "package",
                "myapp_1.0.0_amd64.deb",
                "myapp",
                Some("x86_64-unknown-linux-gnu"),
                None,
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("nfpms:"), "{err}");
        assert!(err.contains("crate 'myapp'"), "{err}");
        assert!(err.contains("conventional default filename"), "{err}");
        assert!(err.contains("{{ .Amd64 }}"), "{err}");
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
        let path = Path::new("dist/macos/app.app");
        let mut first = ArchPathGuard::new();
        first
            .check(
                path,
                "dmgs",
                "image",
                "{{ .ProjectName }}",
                "app.dmg",
                "a",
                None,
                None,
            )
            .expect("scope one first write");
        let mut second = ArchPathGuard::new();
        second
            .check(
                path,
                "dmgs",
                "image",
                "{{ .ProjectName }}",
                "app.dmg",
                "b",
                None,
                None,
            )
            .expect("scope two first write must pass");
    }
}
