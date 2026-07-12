//! Wheel construction: platform-tag derivation from binary inspection and
//! deterministic PEP 427 wheel assembly.
//!
//! The platform tag is derived from the artifact's target triple plus the
//! binary's own bytes — the glibc floor of an ELF, the Mach-O deployment
//! target — so the wheel never claims broader compatibility than the binary
//! actually has. Wheel bytes are deterministic: entries are written in
//! sorted order, deflate-compressed, with every mtime pinned to the commit
//! timestamp.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use sha2::{Digest as _, Sha256};

use super::pep::escape_distribution_name;

/// macOS deployment-target fallback when the Mach-O declares no version
/// load command, keyed by arch: Intel builds default to the 10.12 floor
/// Rust itself targets; arm64 hardware starts at Big Sur.
const MACOS_FALLBACK_X86_64: (u16, u16) = (10, 12);
const MACOS_FALLBACK_ARM64: (u16, u16) = (11, 0);

/// Inspection-derived traits of one built binary, separated from the file
/// I/O so tag derivation is unit-testable on injected values.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BinaryTraits {
    /// Maximum glibc requirement `(major, minor)` found in an ELF's dynamic
    /// symbols. `None` for non-ELF or fully-static binaries.
    pub glibc: Option<(u64, u64)>,
    /// Mach-O minimum macOS version `(major, minor)`. `None` when the load
    /// command is absent or the file is not Mach-O.
    pub macos_min: Option<(u16, u16)>,
    /// `true` for a universal (fat) Mach-O serving multiple arches.
    pub universal: bool,
}

/// Inspect a binary's bytes for the traits [`platform_tag`] consumes.
pub(crate) fn inspect_binary(bytes: &[u8], universal: bool) -> Result<BinaryTraits> {
    let glibc = anodizer_core::libc_check::max_glibc_requirement(bytes)
        .context("pypi: inspect ELF glibc requirement")?
        .map(|v| {
            let mut parts = v.raw().split('.');
            let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(2);
            let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            (major, minor)
        });
    let macos_min = anodizer_core::macho_check::macho_min_os_version(bytes)
        .context("pypi: inspect Mach-O minimum OS version")?;
    Ok(BinaryTraits {
        glibc,
        macos_min,
        universal,
    })
}

/// Map a target-triple architecture to the wheel arch token.
fn wheel_arch(triple: &str) -> Result<&'static str> {
    let arch = triple.split('-').next().unwrap_or_default();
    Ok(match arch {
        "x86_64" => "x86_64",
        "i686" | "i586" => "i686",
        "aarch64" | "arm64" => "aarch64",
        other => bail!(
            "pypi: target '{triple}' has architecture '{other}' with no wheel arch \
             token (supported: x86_64, i686, aarch64)"
        ),
    })
}

/// Derive the wheel platform tag for one binary from its target triple and
/// inspected traits.
///
/// | target family | tag |
/// |---|---|
/// | `*-linux-gnu*` | `manylinux_<glibc maj>_<min>_<arch>` (from the binary's glibc floor) |
/// | `*-linux-musl*` | `musllinux_1_2_<arch>` |
/// | `*-apple-darwin` (thin) | `macosx_<minos maj>_<min>_<x86_64\|arm64>` |
/// | `*-apple-darwin` (fat) | `macosx_<minos maj>_<min>_universal2` |
/// | `x86_64-pc-windows-*` | `win_amd64` |
/// | `i686-pc-windows-*` | `win32` |
/// | `aarch64-pc-windows-*` | `win_arm64` |
///
/// A gnu-target binary with NO glibc requirement errors: every dynamically
/// linked gnu binary imports versioned glibc symbols, so its absence means
/// the artifact under this triple is not the gnu binary it claims to be.
pub(crate) fn platform_tag(triple: &str, traits: &BinaryTraits) -> Result<String> {
    if triple.contains("windows") {
        return Ok(match triple.split('-').next().unwrap_or_default() {
            "x86_64" => "win_amd64".to_string(),
            "i686" | "i586" => "win32".to_string(),
            "aarch64" => "win_arm64".to_string(),
            other => bail!("pypi: windows target '{triple}' (arch '{other}') has no wheel tag"),
        });
    }
    if triple.contains("apple-darwin") {
        let arch = triple.split('-').next().unwrap_or_default();
        let (maj, min) = traits
            .macos_min
            .unwrap_or(if traits.universal || arch == "aarch64" {
                MACOS_FALLBACK_ARM64
            } else {
                MACOS_FALLBACK_X86_64
            });
        let arch_token = if traits.universal {
            "universal2"
        } else {
            match arch {
                "x86_64" => "x86_64",
                "aarch64" | "arm64" => "arm64",
                other => bail!("pypi: darwin target '{triple}' (arch '{other}') has no wheel tag"),
            }
        };
        return Ok(format!("macosx_{maj}_{min}_{arch_token}"));
    }
    if triple.contains("linux") {
        let arch = wheel_arch(triple)?;
        if triple.contains("musl") {
            return Ok(format!("musllinux_1_2_{arch}"));
        }
        if triple.contains("gnu") {
            let Some((maj, min)) = traits.glibc else {
                bail!(
                    "pypi: binary for gnu target '{triple}' declares no GLIBC_* \
                     requirement — a dynamically linked gnu binary always does, so \
                     this looks like the wrong binary for the target"
                );
            };
            return Ok(format!("manylinux_{maj}_{min}_{arch}"));
        }
        bail!("pypi: linux target '{triple}' is neither gnu nor musl — no wheel tag mapping");
    }
    bail!("pypi: target '{triple}' has no wheel platform-tag mapping")
}

/// Everything needed to assemble one wheel, resolved from config + context
/// by the publisher before any file I/O.
#[derive(Debug, Clone)]
pub(crate) struct WheelSpec {
    /// Display-form project name (as configured, e.g. `My-Tool`).
    pub name: String,
    /// PEP 440 version.
    pub version: String,
    /// Wheel platform tag (e.g. `manylinux_2_28_x86_64`).
    pub platform_tag: String,
    /// Executable filename inside `.data/scripts/` (keeps `.exe` on windows).
    pub bin_name: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
    pub requires_python: Option<String>,
    pub keywords: Vec<String>,
    pub classifiers: Vec<String>,
}

impl WheelSpec {
    /// PEP 427 wheel filename: `{escaped}-{version}-py3-none-{tag}.whl`.
    pub(crate) fn filename(&self) -> String {
        format!(
            "{}-{}-py3-none-{}.whl",
            escape_distribution_name(&self.name),
            self.version,
            self.platform_tag
        )
    }

    /// `<escaped>-<version>` prefix shared by the `.data` and `.dist-info`
    /// directories.
    fn prefix(&self) -> String {
        format!("{}-{}", escape_distribution_name(&self.name), self.version)
    }
}

/// Render the wheel's core METADATA (Metadata-Version 2.1).
pub(crate) fn render_metadata(spec: &WheelSpec) -> String {
    let mut out = String::new();
    out.push_str("Metadata-Version: 2.1\n");
    out.push_str(&format!("Name: {}\n", spec.name));
    out.push_str(&format!("Version: {}\n", spec.version));
    if let Some(s) = &spec.summary {
        out.push_str(&format!("Summary: {}\n", s));
    }
    if let Some(h) = &spec.homepage {
        out.push_str(&format!("Project-URL: Homepage, {}\n", h));
    }
    if let Some(l) = &spec.license {
        out.push_str(&format!("License: {}\n", l));
    }
    if !spec.keywords.is_empty() {
        out.push_str(&format!("Keywords: {}\n", spec.keywords.join(",")));
    }
    for c in &spec.classifiers {
        out.push_str(&format!("Classifier: {}\n", c));
    }
    if let Some(r) = &spec.requires_python {
        out.push_str(&format!("Requires-Python: {}\n", r));
    }
    if let Some(d) = &spec.description {
        out.push('\n');
        out.push_str(d);
        if !d.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Render the `WHEEL` file for this build.
pub(crate) fn render_wheel_file(spec: &WheelSpec, generator_version: &str) -> String {
    format!(
        "Wheel-Version: 1.0\nGenerator: anodizer {}\nRoot-Is-Purelib: false\nTag: py3-none-{}\n",
        generator_version, spec.platform_tag
    )
}

/// One `RECORD` row: `path,sha256=<urlsafe-b64-nopad>,<size>`.
fn record_row(path: &str, contents: &[u8]) -> String {
    let digest = Sha256::digest(contents);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    format!("{},sha256={},{}", path, b64, contents.len())
}

/// Convert a unix timestamp to a zip mtime, clamping pre-1980 values (zip's
/// epoch) by returning `None` — the same degrade-to-default the archive
/// stage applies.
fn zip_mtime(ts: Option<u64>) -> Option<zip::DateTime> {
    use chrono::{Datelike as _, TimeZone as _, Timelike as _, Utc};
    let dt = Utc.timestamp_opt(ts? as i64, 0).single()?;
    zip::DateTime::from_date_and_time(
        u16::try_from(dt.year()).ok()?,
        dt.month() as u8,
        dt.day() as u8,
        dt.hour() as u8,
        dt.minute() as u8,
        dt.second() as u8,
    )
    .ok()
}

/// Assemble the wheel at `out_dir/<filename>` and return its path.
///
/// Layout (entries written in sorted-path order, deterministic bytes):
///
/// ```text
/// <name>-<version>.data/scripts/<bin>        (mode 0755)
/// <name>-<version>.dist-info/METADATA
/// <name>-<version>.dist-info/RECORD          (self row last, empty hash/size)
/// <name>-<version>.dist-info/WHEEL
/// ```
pub(crate) fn build_wheel(
    spec: &WheelSpec,
    binary_bytes: &[u8],
    out_dir: &Path,
    mtime: Option<u64>,
    generator_version: &str,
) -> Result<PathBuf> {
    let prefix = spec.prefix();
    let script_path = format!("{prefix}.data/scripts/{}", spec.bin_name);
    let metadata_path = format!("{prefix}.dist-info/METADATA");
    let record_path = format!("{prefix}.dist-info/RECORD");
    let wheel_path = format!("{prefix}.dist-info/WHEEL");

    let metadata = render_metadata(spec);
    let wheel_file = render_wheel_file(spec, generator_version);

    let mut record_rows = vec![
        record_row(&script_path, binary_bytes),
        record_row(&metadata_path, metadata.as_bytes()),
        record_row(&wheel_path, wheel_file.as_bytes()),
    ];
    // The RECORD lists every entry sorted by path, with its own row (empty
    // hash/size) last — the layout pip and maturin both emit.
    record_rows.sort();
    record_rows.push(format!("{record_path},,"));
    let record = format!("{}\n", record_rows.join("\n"));

    // (path, contents, unix mode), sorted by path for deterministic entry
    // order regardless of how the list above is assembled.
    let mut entries: Vec<(&str, &[u8], u32)> = vec![
        (&script_path, binary_bytes, 0o755),
        (&metadata_path, metadata.as_bytes(), 0o644),
        (&record_path, record.as_bytes(), 0o644),
        (&wheel_path, wheel_file.as_bytes(), 0o644),
    ];
    entries.sort_by_key(|(p, _, _)| *p);

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("pypi: create wheel staging dir {}", out_dir.display()))?;
    let out_path = out_dir.join(spec.filename());
    let file = std::fs::File::create(&out_path)
        .with_context(|| format!("pypi: create {}", out_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let mut base = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    if let Some(dt) = zip_mtime(mtime) {
        base = base.last_modified_time(dt);
    }
    for (path, contents, mode) in entries {
        zip.start_file(path, base.unix_permissions(mode))
            .with_context(|| format!("pypi: start wheel entry {path}"))?;
        zip.write_all(contents)
            .with_context(|| format!("pypi: write wheel entry {path}"))?;
    }
    zip.finish().context("pypi: finalize wheel zip")?;
    Ok(out_path)
}
