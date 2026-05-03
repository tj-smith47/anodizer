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
    apply_docker_v2_defaults, build_docker_command, build_docker_v2_command,
    generate_v2_image_tags, is_docker_v2_sbom_enabled, is_docker_v2_skipped, resolve_backend,
    resolve_skip_push,
};

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

pub struct DockerStage;

#[cfg(test)]
mod tests;
