//! Tag handling: naming, discovery, ordering, position, and mutation.
//!
//! The release pipeline asks four separable questions of a repository's tags,
//! and each has its own submodule:
//!
//! - [`family`] — which tags a `tag_template` mints, and which are filtered
//!   out (`ignore_tags`, `ignore_tag_prefixes`, nightly debris). Every other
//!   submodule narrows its candidate set through this one, so a multi-track
//!   workspace (`v`, `core-v`, `operator-v` in one repo) never crosses tracks.
//! - [`discover`] — listing tags and picking the LATEST in a family.
//! - [`previous`] — picking the tag that bounds the START of a release range,
//!   via commit ancestry or the `smartsemver` flat-list ordering.
//! - [`position`] — where a tag sits relative to `HEAD`, which tags point at a
//!   commit, and where history begins.
//! - [`mutate`] — the write side: create, sign, delete, and push tags.

mod discover;
mod family;
mod mutate;
mod position;
mod previous;

#[cfg(test)]
mod tests;

pub(crate) use crate::git::{git_output_in, has_remote_in};

pub use discover::{
    find_latest_tag_matching, find_latest_tag_matching_in, find_latest_tag_matching_with_prefix,
    find_latest_tag_matching_with_prefix_in, get_all_semver_tags, get_all_semver_tags_in,
    get_branch_semver_tags, get_branch_semver_tags_in, list_remote_tag_names_in,
    list_tags_with_prefix,
};
pub(crate) use family::nightly_exclude_describe_args;
pub use family::{
    extract_tag_prefix, filter_ignored_tags, has_version_placeholder, is_nightly_tag,
    per_crate_tag_prefix, render_ignore_patterns, strip_monorepo_prefix, tag_family_glob,
};
pub use mutate::{
    AtomicPushSpec, create_and_push_tag, create_and_push_tag_in, create_tag_local_only,
    delete_local_tag_in, delete_remote_tag_in, push_branch_and_tags_atomic_in,
};
// Production callers of these two sit inside `mutate` itself and reach them
// directly; the module-level alias exists purely so test code can address them
// at `git::tags::…`. Gating it keeps the non-test build free of a dead re-export.
#[cfg(test)]
pub(crate) use mutate::{tag_create_flag, tag_is_signed};
pub use position::{
    TagPosition, get_first_commit, get_first_commit_in, get_tags_at_head, get_tags_at_head_in,
    get_tags_at_sha_in, head_is_at_tag, tag_points_at_head, tag_points_at_head_in, tag_position_in,
};
pub use previous::{
    find_previous_tag, find_previous_tag_in, find_previous_tag_in_family,
    find_previous_tag_in_family_in, find_previous_tag_with_prefix,
    find_previous_tag_with_prefix_in,
};
