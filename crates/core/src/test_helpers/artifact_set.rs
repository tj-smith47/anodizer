//! Reusable [`Artifact`] fixture builder for stage-release / publisher tests.
//!
//! Many `stage-release` and `stage-publish` tests need to hand-write a
//! binary file + optional checksum + signature sidecar onto disk and
//! then register matching [`Artifact`] entries in
//! [`Context::artifacts`](crate::context::Context::artifacts). The
//! repetition makes tests verbose and easy to break with metadata
//! changes (e.g. when a new required key is added to `metadata`).
//!
//! [`TestArtifactSet`] is a builder that materialises the fixture to a
//! `&Path` and returns a `Vec<Artifact>` already populated with
//! filename-derived names, sha256 metadata, format metadata, and a
//! synthetic `url`. Tests can then loop the returned `Vec` into
//! [`ArtifactRegistry::add`](crate::artifact::ArtifactRegistry::add)
//! without bespoke per-test bookkeeping.
//!
//! ## Example
//!
//! ```no_run
//! use anodizer_core::test_helpers::TestContextBuilder;
//! use anodizer_core::test_helpers::artifact_set::TestArtifactSet;
//! use tempfile::TempDir;
//!
//! let tmp = TempDir::new().unwrap();
//! let artifacts = TestArtifactSet::new()
//!     .linux_amd64("demo")
//!     .windows_amd64_zip("demo")
//!     .write_to(tmp.path());
//!
//! let mut ctx = TestContextBuilder::new()
//!     .selected_crates(vec!["demo".to_string()])
//!     .build();
//! for a in artifacts {
//!     ctx.artifacts.add(a);
//! }
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::artifact::{Artifact, ArtifactKind};
use crate::hashing::sha256_file;

/// A single binary-or-archive fixture inside a [`TestArtifactSet`].
///
/// Pass to [`TestArtifactSet::binary`] to customise; the
/// `linux_amd64` / `windows_amd64_zip` convenience constructors on
/// [`TestArtifactSet`] cover the common cases.
#[derive(Debug, Clone)]
pub struct TestBinary {
    /// Owning crate name. Populated into [`Artifact::crate_name`].
    pub crate_name: String,
    /// Rust target triple (e.g. `"x86_64-unknown-linux-gnu"`).
    pub target: String,
    /// File contents to write. SHA-256 is computed from these bytes
    /// after the write so the on-disk file matches the metadata exactly.
    pub bytes: Vec<u8>,
    /// If `true`, also write a `<file>.sha256` sidecar and emit a
    /// matching [`ArtifactKind::Checksum`] entry.
    pub include_checksum: bool,
    /// If `true`, also write a `<file>.sig` sidecar and emit a matching
    /// [`ArtifactKind::Signature`] entry.
    pub include_signature: bool,
}

/// Fixture builder that produces [`Artifact`] entries plus their on-disk
/// files in one step.
#[derive(Debug, Clone, Default)]
pub struct TestArtifactSet {
    binaries: Vec<TestBinary>,
}

impl TestArtifactSet {
    /// Create an empty fixture set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a fully-customised [`TestBinary`] to the fixture.
    pub fn binary(mut self, b: TestBinary) -> Self {
        self.binaries.push(b);
        self
    }

    /// Append a Linux x86_64 binary fixture with a checksum sidecar.
    /// Produces an [`ArtifactKind::Binary`] entry (extension-less file).
    pub fn linux_amd64(self, crate_name: &str) -> Self {
        self.binary(TestBinary {
            crate_name: crate_name.into(),
            target: "x86_64-unknown-linux-gnu".into(),
            bytes: b"#!/bin/sh\necho hi\n".to_vec(),
            include_checksum: true,
            include_signature: false,
        })
    }

    /// Append a Windows x86_64 `.zip` archive fixture with a checksum
    /// sidecar. Produces an [`ArtifactKind::Archive`] entry; the file's
    /// `format` metadata is set to `"zip"` to match what `stage-archive`
    /// emits.
    pub fn windows_amd64_zip(self, crate_name: &str) -> Self {
        let mut zip_bytes: Vec<u8> = b"PK\x03\x04".to_vec();
        zip_bytes.extend_from_slice(&[0u8; 30]);
        self.binary(TestBinary {
            crate_name: crate_name.into(),
            target: "x86_64-pc-windows-msvc".into(),
            bytes: zip_bytes,
            include_checksum: true,
            include_signature: false,
        })
    }

    /// Materialise every binary + sidecar to `dir` and return matching
    /// [`Artifact`] entries with `sha256`, `format`, and `url` metadata
    /// populated. Filenames follow the convention
    /// `<crate>_<target>` (no extension) for plain binaries and
    /// `<crate>_<target>.zip` for zip archives.
    ///
    /// Panics on filesystem errors — callers are unit tests using
    /// `tempfile::TempDir`, where every error is a test bug worth
    /// surfacing immediately.
    pub fn write_to(&self, dir: &Path) -> Vec<Artifact> {
        let mut out = Vec::with_capacity(self.binaries.len() * 3);
        for b in &self.binaries {
            let (filename, kind, format) = filename_kind_format(b);
            let path = dir.join(&filename);

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .unwrap_or_else(|e| panic!("create_dir_all {} failed: {e}", parent.display()));
            }
            fs::write(&path, &b.bytes)
                .unwrap_or_else(|e| panic!("write {} failed: {e}", path.display()));

            let digest = sha256_file(&path)
                .unwrap_or_else(|e| panic!("sha256_file({}) failed: {e}", path.display()));

            let mut metadata: HashMap<String, String> = HashMap::new();
            metadata.insert("sha256".to_string(), digest.clone());
            if let Some(fmt) = format {
                metadata.insert("format".to_string(), fmt.to_string());
            }
            metadata.insert("url".to_string(), synthetic_url(&filename));

            out.push(Artifact {
                kind,
                path: path.clone(),
                name: filename.clone(),
                target: Some(b.target.clone()),
                crate_name: b.crate_name.clone(),
                metadata,
                size: None,
            });

            if b.include_checksum {
                let sum_path = sidecar_path(&path, ".sha256");
                let line = format!("{digest}  {filename}\n");
                fs::write(&sum_path, line.as_bytes())
                    .unwrap_or_else(|e| panic!("write {} failed: {e}", sum_path.display()));
                out.push(Artifact {
                    kind: ArtifactKind::Checksum,
                    path: sum_path.clone(),
                    name: sum_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string(),
                    target: Some(b.target.clone()),
                    crate_name: b.crate_name.clone(),
                    metadata: HashMap::new(),
                    size: None,
                });
            }

            if b.include_signature {
                let sig_path = sidecar_path(&path, ".sig");
                fs::write(
                    &sig_path,
                    b"-----BEGIN SIGNATURE-----\nfake\n-----END SIGNATURE-----\n",
                )
                .unwrap_or_else(|e| panic!("write {} failed: {e}", sig_path.display()));
                out.push(Artifact {
                    kind: ArtifactKind::Signature,
                    path: sig_path.clone(),
                    name: sig_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string(),
                    target: Some(b.target.clone()),
                    crate_name: b.crate_name.clone(),
                    metadata: HashMap::new(),
                    size: None,
                });
            }
        }
        out
    }
}

/// Choose `(filename, ArtifactKind, format)` from the bytes shape and
/// target. A zip archive is detected by the `PK\x03\x04` magic bytes;
/// everything else is treated as a plain binary.
fn filename_kind_format(b: &TestBinary) -> (String, ArtifactKind, Option<&'static str>) {
    if b.bytes.starts_with(b"PK\x03\x04") {
        (
            format!("{}_{}.zip", b.crate_name, b.target),
            ArtifactKind::Archive,
            Some("zip"),
        )
    } else {
        (
            format!("{}_{}", b.crate_name, b.target),
            ArtifactKind::Binary,
            None,
        )
    }
}

/// Append `suffix` (e.g. `.sha256`, `.sig`) to a path's filename,
/// preserving the parent directory.
fn sidecar_path(base: &Path, suffix: &str) -> PathBuf {
    let mut name = base
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("artifact")
        .to_string();
    name.push_str(suffix);
    base.with_file_name(name)
}

/// Synthetic URL for the `url` metadata key. The host `example.test` is
/// reserved per RFC 6761 so it cannot escape into a real HTTP request.
fn synthetic_url(filename: &str) -> String {
    format!("https://example.test/dl/{filename}")
}

#[cfg(test)]
mod self_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_to_materialises_files_at_expected_paths() {
        let tmp = TempDir::new().unwrap();
        let artifacts = TestArtifactSet::new()
            .linux_amd64("demo")
            .write_to(tmp.path());

        // Binary + checksum sidecar.
        assert_eq!(artifacts.len(), 2, "binary + .sha256 expected");
        let binary = &artifacts[0];
        let checksum = &artifacts[1];

        assert_eq!(binary.kind, ArtifactKind::Binary);
        assert_eq!(checksum.kind, ArtifactKind::Checksum);

        assert!(
            binary.path.exists(),
            "binary file should exist: {:?}",
            binary.path
        );
        assert!(
            checksum.path.exists(),
            "sha256 file should exist: {:?}",
            checksum.path
        );
        assert_eq!(binary.name, "demo_x86_64-unknown-linux-gnu");
        assert_eq!(checksum.name, "demo_x86_64-unknown-linux-gnu.sha256");
    }

    #[test]
    fn sha256_metadata_matches_on_disk_file() {
        let tmp = TempDir::new().unwrap();
        let artifacts = TestArtifactSet::new()
            .linux_amd64("demo")
            .write_to(tmp.path());

        let binary = &artifacts[0];
        let recomputed = sha256_file(&binary.path).expect("sha256_file");
        assert_eq!(
            binary.metadata.get("sha256").map(String::as_str),
            Some(recomputed.as_str()),
            "metadata sha256 must match on-disk file"
        );
    }

    #[test]
    fn linux_and_windows_zip_produce_expected_kinds_and_targets() {
        let tmp = TempDir::new().unwrap();
        let artifacts = TestArtifactSet::new()
            .linux_amd64("demo")
            .windows_amd64_zip("demo")
            .write_to(tmp.path());

        // 2 binaries × (file + .sha256) = 4 artifacts.
        assert_eq!(artifacts.len(), 4);

        let linux = artifacts
            .iter()
            .find(|a| {
                a.target.as_deref() == Some("x86_64-unknown-linux-gnu")
                    && a.kind == ArtifactKind::Binary
            })
            .expect("linux binary present");
        let windows = artifacts
            .iter()
            .find(|a| {
                a.target.as_deref() == Some("x86_64-pc-windows-msvc")
                    && a.kind == ArtifactKind::Archive
            })
            .expect("windows archive present");

        assert_eq!(linux.kind, ArtifactKind::Binary);
        assert_eq!(
            linux.metadata.get("format"),
            None,
            "plain binary has no format"
        );
        assert!(linux.metadata.contains_key("url"));

        assert_eq!(windows.kind, ArtifactKind::Archive);
        assert_eq!(
            windows.metadata.get("format").map(String::as_str),
            Some("zip"),
            "windows zip should have format=zip"
        );
        assert!(windows.name.ends_with(".zip"));
    }
}
