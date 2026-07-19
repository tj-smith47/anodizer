//! NPM registry publisher.
//!
//! Two distribution modes (see [`anodizer_core::config::NpmMode`]):
//! * `optional-deps` (default for a Rust release): emits npm's native
//!   per-platform packages + a metapackage whose `optionalDependencies` list
//!   them; npm's `os`/`cpu`/`libc` resolution installs only the matching
//!   prebuilt — no download, no postinstall. The pattern leading Rust CLIs
//!   ship binaries through npm with (biome, git-cliff).
//! * `postinstall`: emits a single `package.json` + `postinstall.js` shim that
//!   downloads + sha256-verifies the OS/arch-matching archive at install time.
//!
//! The artifacts already exist (release archives + per-target binaries from
//! the build/archive stages); this publisher wraps them in `npm i`-installable
//! packages and pushes to the configured registry (default
//! `https://registry.npmjs.org`).

mod auth;
mod manifest;
mod optional_deps;
mod platform_render;
mod promote;
pub mod publish;
pub mod publisher;
mod staging;

#[cfg(test)]
mod tests;

pub use promote::{NpmPromoter, preflight as npm_promote_preflight};
pub use publish::{NpmTarget, publish_to_npm};
pub use publisher::NpmPublisher;

/// Whether `<package>@<version>` is visible on `registry` — an anonymous
/// metadata `GET <registry>/<encoded name>/<version>` answered 200.
///
/// The public counterpart of the preflight duplicate-version probe, sharing
/// its HTTP client, retry envelope, and scoped-name encoding so "visible on
/// the registry" means the same thing before and after a publish.
///
/// `Ok(true)` = a 200 (the version answers), `Ok(false)` = a definitive 404
/// (absent), `Err` = the registry could not be consulted (5xx, transport
/// failure). Landing verification must NOT treat that `Err` as "not visible":
/// an npm version is immutable once published, so folding an outage into a
/// hard finding would fail an already-landed, one-way-door release.
pub fn version_visible_on_registry(
    registry: &str,
    package: &str,
    version: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> anyhow::Result<bool> {
    let url = format!(
        "{}/{}/{}",
        registry.trim_end_matches('/'),
        publish::encode_package_path(package),
        version,
    );
    crate::publisher_preflight::probe_version_landing(&url, "npm landing probe", policy, log)
}

/// Context-free entry crate name for the top-level `npms:` block: the primary
/// crate name, falling back to the project name — the same fallback the
/// publisher applies (`npms:` entries carry no per-crate association). Used by
/// `tag rollback`'s burn probe to map a tag's version onto the npm package
/// outside a release run.
pub fn static_entry_crate_name(config: &anodizer_core::config::Config) -> String {
    config
        .primary_crate_name()
        .map(str::to_string)
        .unwrap_or_else(|| config.project_name.clone())
}

/// Static (context-free) published package name for the rollback burn probe:
/// the postinstall package name, or the optional-deps metapackage name —
/// resolved without a render context ([`manifest::resolve_name`] /
/// [`optional_deps::resolve_metapackage`]). Returns `None` when that name is a
/// template expression: outside a release run there is nothing to render it
/// with, and a destructive rollback that cannot name the immutable package it
/// would orphan must fail closed rather than probe a guessed name (same
/// posture as chocolatey's `static_package_id`).
///
/// Also returns `None` in `skip_metapackage` optional-deps mode: there the
/// metapackage is NEVER published — only the per-platform
/// `<name>-<os>-<arch>` packages are, and their names derive from a render
/// context plus the built artifacts this context-free probe deliberately
/// lacks (`render_platform_name` needs a render context and a per-target
/// triple). Naming the never-published metapackage here would probe
/// a package that returns 404 and read a false "clean", letting a same-version
/// re-cut poison the burned per-platform slots. Failing closed (`None` → the
/// rollback guard's unresolvable branch refuses) is the only safe verdict. A
/// templated `skip_metapackage` is likewise unresolvable statically → `None`.
///
/// Public for the same reason as [`version_visible_on_registry`]: `tag
/// rollback`'s published-state guard must name the same package the publisher
/// would push.
pub fn static_published_name(
    crate_name: &str,
    cfg: &anodizer_core::config::NpmConfig,
) -> Option<String> {
    let name = match cfg.mode {
        anodizer_core::config::NpmMode::Postinstall => manifest::resolve_name(cfg, crate_name),
        anodizer_core::config::NpmMode::OptionalDeps => match cfg.skip_metapackage.as_ref() {
            // Templated or truthy skip_metapackage → the metapackage isn't the
            // published unit; fail closed (see the doc above).
            Some(s) if s.is_template() || s.as_bool() => return None,
            _ => optional_deps::resolve_metapackage(cfg, crate_name),
        },
    };
    (!name.contains("{{")).then(|| name.to_string())
}

/// Static (context-free) registry endpoint for the rollback burn probe:
/// `cfg.registry` when set and not templated (trailing slash trimmed), else
/// the default npm registry. Returns `None` when `cfg.registry` is a template
/// expression — its host is unknown outside a release run, so the guard cannot
/// name where the package would land and must fail closed.
pub fn static_registry(cfg: &anodizer_core::config::NpmConfig) -> Option<String> {
    match cfg
        .registry
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(r) if r.contains("{{") => None,
        Some(r) => Some(r.trim_end_matches('/').to_string()),
        None => Some(manifest::DEFAULT_REGISTRY.to_string()),
    }
}

#[cfg(test)]
mod version_visible_tests {
    use super::*;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };

    fn policy() -> anodizer_core::retry::RetryPolicy {
        anodizer_core::retry::RetryPolicy {
            max_attempts: 1,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        }
    }

    fn logger() -> anodizer_core::log::StageLogger {
        let ctx = anodizer_core::context::Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        ctx.logger("test")
    }

    #[test]
    fn scoped_name_probes_encoded_version_route() {
        // The scoped package's `/` must reach the registry as `%2F`; a 200 on
        // exactly that route proves both the encoding and the URL shape.
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/@scope%2Fapp/1.2.3",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        assert!(
            version_visible_on_registry(
                &format!("http://{addr}/"),
                "@scope/app",
                "1.2.3",
                &policy(),
                &logger(),
            )
            .unwrap()
        );
    }

    #[test]
    fn static_published_name_resolves_by_mode_and_rejects_template() {
        use anodizer_core::config::{NpmConfig, NpmMode};
        // optional-deps → metapackage (cfg.metapackage else cfg.name else crate).
        assert_eq!(
            super::static_published_name(
                "mycrate",
                &NpmConfig {
                    metapackage: Some("biome".into()),
                    ..Default::default()
                }
            ),
            Some("biome".to_string())
        );
        assert_eq!(
            super::static_published_name("mycrate", &NpmConfig::default()),
            Some("mycrate".to_string())
        );
        // postinstall → the single `name:` package (else crate).
        assert_eq!(
            super::static_published_name(
                "mycrate",
                &NpmConfig {
                    mode: NpmMode::Postinstall,
                    name: Some("@scope/app".into()),
                    ..Default::default()
                }
            ),
            Some("@scope/app".to_string())
        );
        // A templated name is unresolvable outside a release run → fail closed.
        assert_eq!(
            super::static_published_name(
                "mycrate",
                &NpmConfig {
                    metapackage: Some("{{ .ProjectName }}".into()),
                    ..Default::default()
                }
            ),
            None
        );
    }

    #[test]
    fn static_registry_defaults_trims_and_rejects_template() {
        use anodizer_core::config::NpmConfig;
        let def = super::static_registry(&NpmConfig::default()).expect("default registry");
        assert!(
            def.contains("registry.npmjs.org"),
            "default is public npm: {def}"
        );
        // Explicit value has its trailing slash trimmed.
        assert_eq!(
            super::static_registry(&NpmConfig {
                registry: Some("https://npm.internal.example/".into()),
                ..Default::default()
            }),
            Some("https://npm.internal.example".to_string())
        );
        // Templated registry → unresolvable host → fail closed.
        assert_eq!(
            super::static_registry(&NpmConfig {
                registry: Some("https://{{ .Env.REG }}/".into()),
                ..Default::default()
            }),
            None
        );
    }

    #[test]
    fn static_entry_crate_name_prefers_primary_then_project() {
        use anodizer_core::config::{Config, CrateConfig};
        let mut config = Config {
            project_name: "proj".into(),
            ..Default::default()
        };
        config.crates = vec![CrateConfig {
            name: "primary".into(),
            ..Default::default()
        }];
        assert_eq!(super::static_entry_crate_name(&config), "primary");
        config.crates.clear();
        assert_eq!(super::static_entry_crate_name(&config), "proj");
    }

    #[test]
    fn missing_version_is_absent_not_indeterminate() {
        // An unmatched route 404s: a definitive absence, distinct from an
        // unreachable registry, so the probe reports Ok(false) not Err.
        let (addr, _log) = spawn_scripted_responder(Vec::new());
        assert!(
            !version_visible_on_registry(
                &format!("http://{addr}"),
                "app",
                "9.9.9",
                &policy(),
                &logger(),
            )
            .unwrap()
        );
    }
}
