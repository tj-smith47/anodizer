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
//! Construct one [`ArchPathGuard`] per (crate, config) scope and call
//! [`ArchPathGuard::check`] with each rendered output path. The first
//! duplicate returns an error naming the offending template and crate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Tracks the output paths a single per-architecture artifact loop has
/// produced, erroring on the first collision.
#[derive(Debug, Default)]
pub struct ArchPathGuard {
    seen: HashSet<PathBuf>,
}

impl ArchPathGuard {
    /// A fresh guard with no recorded paths. One per (crate, config) scope.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `path` for this scope; error if a previous call already
    /// produced it.
    ///
    /// `stage` is the config key (`"dmgs"`), `artifact` the user-facing noun
    /// (`"image"`) — pluralized with a trailing `s` in the message,
    /// `name_template` the offending template, `rendered` the rendered output
    /// name, and `crate_name` the crate being built.
    pub fn check(
        &mut self,
        path: &Path,
        stage: &str,
        artifact: &str,
        name_template: &str,
        rendered: &str,
        crate_name: &str,
    ) -> anyhow::Result<()> {
        if self.seen.insert(path.to_path_buf()) {
            return Ok(());
        }
        anyhow::bail!(
            "{stage}: name template '{name_template}' rendered the same {artifact} \
             '{rendered}' more than once for crate '{crate_name}', so one build target \
             would silently overwrite another. Add '{{{{ .Arch }}}}' to the `name` \
             (e.g. \"{{{{ .ProjectName }}}}_{{{{ .Arch }}}}\") so each build target's \
             {artifact} gets a distinct path."
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
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("app_bundles:"), "{err}");
        assert!(err.contains("crate 'app'"), "{err}");
        assert!(err.contains("{{ .Arch }}"), "{err}");
        assert!(err.contains("bundle gets a distinct path"), "{err}");
    }

    #[test]
    fn separate_scopes_do_not_share_state() {
        // Two crates (or two config entries) each render the same leaf path;
        // a per-scope guard must NOT treat the second scope's first write as
        // a collision.
        let path = Path::new("dist/macos/app.app");
        let mut first = ArchPathGuard::new();
        first
            .check(path, "dmgs", "image", "{{ .ProjectName }}", "app.dmg", "a")
            .expect("scope one first write");
        let mut second = ArchPathGuard::new();
        second
            .check(path, "dmgs", "image", "{{ .ProjectName }}", "app.dmg", "b")
            .expect("scope two first write must pass");
    }
}
