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

mod manifest;
mod optional_deps;
pub mod publish;
pub mod publisher;

#[cfg(test)]
mod tests;

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
