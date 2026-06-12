// ---------------------------------------------------------------------------
// levenshtein_distance / find_image_digest
// ---------------------------------------------------------------------------
mod spelling;

// ---------------------------------------------------------------------------
// is_retriable_error / docker_supports_provenance / check_buildx_driver
// ---------------------------------------------------------------------------
mod detect;
pub use detect::{is_retriable_error, is_retriable_error_v2};

// ---------------------------------------------------------------------------
// platform_to_arch / tag_suffix
// ---------------------------------------------------------------------------
mod platform;
pub use platform::platform_to_arch;

// ---------------------------------------------------------------------------
// parse_duration_string / resolve_retry_params
// ---------------------------------------------------------------------------
mod retry;
pub use retry::{parse_duration_string, resolve_retry_params};

// ---------------------------------------------------------------------------
// build_docker_command / build_docker_v2_command / V2 resolve helpers / skip_push
// ---------------------------------------------------------------------------
mod command;
pub use command::{
    BUILDX_ONLY_FLAGS, DockerV1Spec, DockerV2Spec, apply_docker_v2_defaults, build_docker_command,
    build_docker_v2_command, build_podman_push_commands, enforce_podman_linux_only,
    generate_v2_image_tags, is_docker_v2_sbom_enabled, is_docker_v2_skipped, resolve_backend,
    resolve_skip_push, validate_podman_flag_compat,
};

// ---------------------------------------------------------------------------
// parse_base_image / get_base_image — Dockerfile FROM resolver feeding
// the `{{ .BaseImage }}` / `{{ .BaseImageDigest }}` template surface.
// ---------------------------------------------------------------------------
mod baseimage;
pub use baseimage::{BaseImage, get_base_image, parse_base_image};

// ---------------------------------------------------------------------------
// DockerBuildJob / DockerBuildResult / execute_docker_build
// ---------------------------------------------------------------------------
mod build;

// ---------------------------------------------------------------------------
// stage_artifacts_v2 / copy_dockerfile / warn_project_markers / stage_extra_files
// ---------------------------------------------------------------------------
mod staging;
pub use staging::PROJECT_MARKERS;

// ---------------------------------------------------------------------------
// DockerStage
// ---------------------------------------------------------------------------
mod run;

use std::sync::Arc;

use detect::BuildxVersionProbe;

/// Probe closure used by `DockerStage` to classify the buildx-version probe
/// outcome. Production code leaves the override unset; tests inject a
/// deterministic closure that pins the probe-invocation contract end-to-end
/// without spawning `docker`.
pub(crate) type BuildxVersionProbeFn = dyn Fn() -> BuildxVersionProbe + Send + Sync;

pub struct DockerStage {
    /// Optional override for the buildx-version probe used by `Stage::run`.
    ///
    /// `None` => the live probe (`detect::probe_buildx_version`) is used and
    /// the stage shells out to `docker buildx version`. `Some` => tests
    /// inject a deterministic closure that returns a chosen
    /// `BuildxVersionProbe` outcome without spawning a subprocess.
    probe: Option<Arc<BuildxVersionProbeFn>>,
}

impl DockerStage {
    /// Construct a `DockerStage` that uses the live buildx-version probe.
    pub fn new() -> Self {
        Self { probe: None }
    }

    /// Test-only constructor: inject a buildx-version probe closure that the
    /// stage will call in place of `detect::probe_buildx_version`. The
    /// closure is invoked at most once per `Stage::run` invocation, and only
    /// when the probe gate fires (non-dry-run + at least one crate carries a
    /// `docker_v2` config).
    #[cfg(test)]
    pub(crate) fn with_probe(probe: Arc<BuildxVersionProbeFn>) -> Self {
        Self { probe: Some(probe) }
    }
}

impl Default for DockerStage {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;

/// Environment requirements for the docker stage: the `docker` CLI plus a
/// reachable daemon whenever any crate declares `dockers_v2:`.
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    let any = anodizer_core::env_preflight::crate_universe(&ctx.config)
        .into_iter()
        .flat_map(|c| c.dockers_v2.iter().flatten())
        .any(|d| {
            !d.skip.as_ref().is_some_and(|v| {
                v.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .unwrap_or(false)
            })
        });
    if !any {
        return Vec::new();
    }
    vec![
        anodizer_core::EnvRequirement::Tool {
            name: "docker".to_string(),
        },
        anodizer_core::EnvRequirement::DockerDaemon,
    ]
}
