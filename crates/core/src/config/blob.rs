use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    ExtraFileSpec, StringOrBool, TemplatedExtraFile, deserialize_string_or_bool_opt,
    deserialize_string_or_vec_opt,
};

// ---------------------------------------------------------------------------
// BlobConfig (S3/GCS/Azure cloud storage)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BlobConfig {
    /// Unique identifier for this blob config.
    pub id: Option<String>,
    /// Cloud storage provider: s3, gcs (or gs), or azblob (or azure).
    pub provider: String,
    /// Bucket or container name (supports templates).
    pub bucket: String,
    /// Directory/folder within the bucket (supports templates).
    /// Default: `{{ ProjectName }}/{{ Tag }}`.
    pub directory: Option<String>,
    /// AWS region (S3 only).
    pub region: Option<String>,
    /// Custom endpoint URL for S3-compatible storage (e.g. MinIO, R2, DO Spaces).
    pub endpoint: Option<String>,
    /// Disable SSL for the connection (S3 only, default: false).
    pub disable_ssl: Option<bool>,
    /// Enable path-style addressing for S3-compatible backends.
    /// Defaults to `true` when `endpoint` is set (MinIO, R2, DO Spaces need this),
    /// `false` otherwise (standard AWS virtual-hosted style).
    pub s3_force_path_style: Option<bool>,
    /// Canned ACL for uploaded objects.
    /// **S3**: one of `private` (default), `public-read`, `public-read-write`,
    /// `authenticated-read`, `aws-exec-read`, `bucket-owner-read`,
    /// `bucket-owner-full-control`. The accepted ACL set;
    /// AWS's `log-delivery-write` is intentionally omitted because it is only
    /// valid on `S3LogBucket` targets and would silently fail on a normal bucket.
    /// **GCS**: pass the camelCase predefined-ACL name (e.g. `publicRead`,
    /// `bucketOwnerFullControl`); not validated up-front, so a typo surfaces
    /// as a 400 from the GCS API at upload time.
    pub acl: Option<String>,
    /// HTTP Cache-Control header values, joined with ", " when uploading.
    /// Accepts a string (single value) or array of strings in YAML.
    #[serde(deserialize_with = "deserialize_string_or_vec_opt", default)]
    pub cache_control: Option<Vec<String>>,
    /// HTTP Content-Disposition header (supports templates).
    /// Default: `"attachment;filename={{Filename}}"`. Set to `"-"` to disable.
    pub content_disposition: Option<String>,
    /// AWS KMS encryption key for server-side encryption (S3 only).
    pub kms_key: Option<String>,
    /// Build IDs to include. Empty means all artifacts.
    pub ids: Option<Vec<String>>,
    /// Skip this blob config. Accepts bool or template string
    /// (e.g. `"{{ if IsSnapshot }}true{{ endif }}"` for conditional skip).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Also upload metadata.json and artifacts.json.
    pub include_meta: Option<bool>,
    /// Pre-existing files to upload (supports glob patterns).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before upload.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ Tag }}` are expanded.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Upload only extra files (skip artifacts).
    pub extra_files_only: Option<bool>,
    /// Maximum number of parallel uploads for this blob config.
    /// Overrides the global `--parallelism` setting when set.
    pub parallelism: Option<usize>,
    /// When `true`, a failed blob upload counts as a required-publisher
    /// failure: trips the submitter gate (chocolatey/winget/snapcraft/aur
    /// don't run if blob failed) and surfaces in
    /// `report.required_failures()` so the CLI exits non-zero.
    ///
    /// Default: `false`. Snapshot release pipelines typically don't
    /// require blobs; production pipelines that ship binaries via S3/GCS
    /// often do.
    ///
    /// When multiple blob configs exist, the blob stage records ONE
    /// aggregated `PublisherResult`: `required = true` if ANY config opted
    /// in. Same semantics as "any required blob target failing should
    /// fail the release."
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the blob config is skipped.
    /// Render failure hard-errors. The `blobs[].if:` conditional gate.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}
