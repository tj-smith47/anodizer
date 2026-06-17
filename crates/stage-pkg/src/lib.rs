use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

// ---------------------------------------------------------------------------
// pkgbuild_command
// ---------------------------------------------------------------------------

/// Construct the `pkgbuild` CLI command arguments.
///
/// Returns args suitable for `Command::new(&args[0]).args(&args[1..])`.
pub fn pkgbuild_command(
    staging_dir: &str,
    identifier: &str,
    version: &str,
    install_location: &str,
    scripts: Option<&str>,
    min_os_version: Option<&str>,
    output_path: &str,
) -> Vec<String> {
    let mut args = vec![
        "pkgbuild".to_string(),
        "--root".to_string(),
        staging_dir.to_string(),
        "--identifier".to_string(),
        identifier.to_string(),
        "--version".to_string(),
        version.to_string(),
        "--install-location".to_string(),
        install_location.to_string(),
    ];

    if let Some(scripts_dir) = scripts {
        args.push("--scripts".to_string());
        args.push(scripts_dir.to_string());
    }

    if let Some(min_os) = min_os_version {
        args.push("--min-os-version".to_string());
        args.push(min_os.to_string());
    }

    args.push(output_path.to_string());
    args
}

// ---------------------------------------------------------------------------
// Tool resolution
// ---------------------------------------------------------------------------

/// Which build path produces the flat `.pkg`.
///
/// Resolved once per config entry from PATH so the per-binary loop can dispatch
/// without re-probing. `pkgbuild` (Apple/Xcode, macOS-only) is preferred when
/// present; otherwise the Linux flat-package toolchain (`xar` + `mkbom` +
/// `cpio`, with gzip done in-process) assembles the identical XAR layout by
/// hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgBuilder {
    /// Native Apple `pkgbuild` on PATH.
    Pkgbuild,
    /// Linux-native flat XAR assembly via `xar`/`mkbom`/`cpio` (gzip in-process).
    Linux,
}

/// The Linux flat-package toolchain, all of which must be present together for
/// [`PkgBuilder::Linux`]. gzip is NOT listed: the Payload is gzipped in-process
/// (flate2) so the compressed stream is byte-stable, so no `gzip` binary is
/// spawned or required.
const LINUX_PKG_TOOLS: [&str; 3] = ["xar", "mkbom", "cpio"];

/// Resolve which `.pkg` build path is available on this host.
///
/// `probe` reports whether a tool name resolves on PATH (injectable so the
/// resolution logic is unit-testable without a real PATH). Returns the actionable
/// error string naming BOTH options when neither path is satisfiable.
pub fn resolve_pkg_builder(probe: impl Fn(&str) -> bool) -> Result<PkgBuilder, String> {
    if probe("pkgbuild") {
        return Ok(PkgBuilder::Pkgbuild);
    }
    if LINUX_PKG_TOOLS.iter().all(|t| probe(t)) {
        return Ok(PkgBuilder::Linux);
    }
    Err(
        "neither `pkgbuild` (macOS, `xcode-select --install`) nor the Linux \
         flat-package toolchain (`xar` + `mkbom`/bomutils + `cpio`) is \
         available; install one to build .pkg installers"
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Linux flat-package builder
// ---------------------------------------------------------------------------

/// Recursively apply `mtime` to every file and directory under `root`.
///
/// [`anodizer_core::util::apply_mod_timestamp`] only touches top-level regular
/// files; the flat-package `pkgroot` nests the payload under the install
/// location, so deterministic `Payload` mtimes require a recursive walk over
/// files AND directories. `filetime::set_file_mtime` uses `utimensat` rather
/// than `open(O_WRONLY)`, so it stamps directories (which `open(O_WRONLY)`
/// rejects with EISDIR) — leaving directory mtimes at wall-clock would leak
/// non-reproducible bytes into the cpio Payload.
fn apply_mtime_recursive(root: &std::path::Path, mtime: std::time::SystemTime) -> Result<()> {
    let ft_time = filetime::FileTime::from_system_time(mtime);
    for entry in
        fs::read_dir(root).with_context(|| format!("read pkgroot dir {}", root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            apply_mtime_recursive(&path, mtime)?;
        }
        filetime::set_file_mtime(&path, ft_time)
            .with_context(|| format!("set mtime on {}", path.display()))?;
    }
    // Stamp `root` itself last: the cpio archive includes the payload root as
    // its `.` entry, whose mtime must be pinned too or the Payload (and the
    // .pkg) drifts by the directory's wall-clock creation time every build.
    filetime::set_file_mtime(root, ft_time)
        .with_context(|| format!("set mtime on {}", root.display()))?;
    Ok(())
}

/// Count regular files and total byte size under `root` (recursive).
///
/// Feeds `PackageInfo`'s `installKBytes`/`numberOfFiles`, mirroring what
/// `pkgbuild` records from the same payload tree.
fn payload_stats(root: &std::path::Path) -> Result<(u64, u64)> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in
        fs::read_dir(root).with_context(|| format!("stat pkgroot dir {}", root.display()))?
    {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            let (f, b) = payload_stats(&entry.path())?;
            files += f;
            bytes += b;
        } else if ft.is_file() {
            files += 1;
            bytes += entry.metadata()?.len();
        }
    }
    Ok((files, bytes))
}

/// XML-escape a value destined for an attribute or text node.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Build a byte-reproducible gzipped `cpio` (odc) archive of `src_root`'s
/// contents into `dest`.
///
/// Drives `find . | LC_ALL=C sort | cpio -o --format odc -R 0:0` from inside
/// `src_root` via a single `sh -c` invocation, failing if any stage does
/// (`set -o pipefail`), capturing the raw cpio on stdout. The cwd-relative
/// `find .` keeps archived paths rooted at `.` exactly as `pkgbuild` emits
/// them. Byte-stability guards make the Payload identical across CI runners
/// and match native pkgbuild:
/// - `LC_ALL=C sort` fixes entry order (raw readdir order varies by host).
/// - `-R 0:0` forces uid/gid to root, removing the runner's ownership.
/// - [`normalize_odc_cpio`] zeroes the per-entry `dev`/`ino` header fields,
///   which the odc format encodes from the live filesystem and which differ
///   every build — the dominant source of `.pkg` drift before this fix.
/// - the gzip is produced in-process with flate2 (mtime=0, no embedded OS
///   byte), not shell `gzip`, so the compressed stream is itself stable.
///
/// Payload mtimes are already pinned by [`apply_mtime_recursive`] (including
/// the `.` root) before this runs, so the cpio header `mtime` fields are fixed.
fn cpio_gzip_archive(
    src_root: &std::path::Path,
    dest: &std::path::Path,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let pipeline = "set -o pipefail; find . | LC_ALL=C sort | \
         cpio -o --format odc -R 0:0 2>/dev/null";
    let output = Command::new("sh")
        .arg("-c")
        .arg(pipeline)
        .current_dir(src_root)
        .output()
        .with_context(|| format!("execute cpio pipeline for {}", src_root.display()))?;
    if !output.status.success() {
        log.check_output(output, "cpio")?;
        return Ok(());
    }
    let normalized = normalize_odc_cpio(&output.stdout);

    use flate2::{Compression, write::GzEncoder};
    use std::io::Write as _;
    let file =
        fs::File::create(dest).with_context(|| format!("create Payload {}", dest.display()))?;
    let mut enc = GzEncoder::new(file, Compression::best());
    enc.write_all(&normalized)
        .with_context(|| format!("gzip Payload {}", dest.display()))?;
    enc.finish()
        .with_context(|| format!("finalize Payload {}", dest.display()))?;
    Ok(())
}

/// Zero the `dev` (bytes 6..12) and `ino` (bytes 12..18) fields of every odc
/// cpio header in `data`, returning the rewritten archive.
///
/// The POSIX portable (`odc`) cpio header is 76 ASCII-octal bytes followed by
/// the NUL-terminated name and the file body. The layout is fixed-width:
/// `magic[6] dev[6] ino[6] mode[6] uid[6] gid[6] nlink[6] rdev[6] mtime[11]
/// namesize[6] filesize[11]`. cpio fills `dev`/`ino` from the staging
/// filesystem's live inode numbers, which change on every build and are the
/// primary reason two otherwise-identical Payloads differ byte-for-byte.
/// pkgbuild's Payloads carry zeroed inode identity, so zeroing here both fixes
/// reproducibility and matches Apple's output. The walk stops at the
/// `TRAILER!!!` sentinel; trailing zero-padding after it is preserved verbatim.
fn normalize_odc_cpio(data: &[u8]) -> Vec<u8> {
    const HDR: usize = 76;
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0usize;
    while i + HDR <= data.len() {
        if &data[i..i + 6] != b"070707" {
            // Not on a header boundary (unexpected) — copy the remainder as-is
            // rather than corrupt the archive.
            out.extend_from_slice(&data[i..]);
            return out;
        }
        let parse = |off: usize, len: usize| -> Option<usize> {
            std::str::from_utf8(&data[off..off + len])
                .ok()
                .and_then(|s| usize::from_str_radix(s.trim(), 8).ok())
        };
        let (namesize, filesize) = match (parse(i + 59, 6), parse(i + 65, 11)) {
            (Some(n), Some(f)) => (n, f),
            _ => {
                out.extend_from_slice(&data[i..]);
                return out;
            }
        };
        let mut hdr = data[i..i + HDR].to_vec();
        hdr[6..12].copy_from_slice(b"000000"); // dev
        hdr[12..18].copy_from_slice(b"000000"); // ino
        out.extend_from_slice(&hdr);
        let name_start = i + HDR;
        let body_start = name_start + namesize;
        let next = body_start + filesize;
        if next > data.len() {
            out.extend_from_slice(&data[name_start..]);
            return out;
        }
        out.extend_from_slice(&data[name_start..next]);
        let name = &data[name_start..body_start];
        if name.trim_ascii_end().ends_with(b"TRAILER!!!") || name.starts_with(b"TRAILER!!!") {
            out.extend_from_slice(&data[next..]); // preserve block padding
            return out;
        }
        i = next;
    }
    out.extend_from_slice(&data[i..]);
    out
}

/// Assemble a flat XAR `.pkg` installer without Apple's `pkgbuild`.
///
/// Replicates the exact layout `pkgbuild`/`productbuild` emit — a XAR archive
/// containing a top-level `Distribution` and a `base.pkg/` component
/// (`PackageInfo`, `Bom`, `Payload`, and `Scripts` when present). `staging_dir`
/// holds the payload flat (as the native path stages it); this routine rebuilds
/// it under `install_location` so the `Bom` and `Payload` encode the real
/// install destination, matching `pkgbuild --root <staging> --install-location`.
#[allow(clippy::too_many_arguments)]
pub fn build_flat_pkg_linux(
    staging_dir: &std::path::Path,
    identifier: &str,
    version: &str,
    install_location: &str,
    scripts: Option<&str>,
    min_os_version: Option<&str>,
    mod_timestamp: Option<&str>,
    pkg_path: &std::path::Path,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let work = tempfile::tempdir().context("create temp work dir for flat pkg")?;
    let pkgroot = work.path().join("pkgroot");
    let flatdir = work.path().join("flat");
    let component = flatdir.join("base.pkg");
    fs::create_dir_all(&component)
        .with_context(|| format!("create flat component dir {}", component.display()))?;

    // Mirror `--root <staging> --install-location <loc>`: the payload must live
    // UNDER the install location inside pkgroot so Bom/Payload carry the real
    // destination path, not a flat `./binary`.
    let rel_install = install_location.trim_start_matches('/');
    let payload_dest = if rel_install.is_empty() {
        pkgroot.clone()
    } else {
        pkgroot.join(rel_install)
    };
    fs::create_dir_all(&payload_dest)
        .with_context(|| format!("create payload dest {}", payload_dest.display()))?;

    for entry in fs::read_dir(staging_dir)
        .with_context(|| format!("read staging dir {}", staging_dir.display()))?
    {
        let entry = entry?;
        let src = entry.path();
        let dst = payload_dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            anodizer_core::util::copy_dir_tree(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)
                .with_context(|| format!("copy payload {} -> {}", src.display(), dst.display()))?;
        }
    }

    let (num_files, total_bytes) = payload_stats(&pkgroot)?;
    let install_kbytes = total_bytes.div_ceil(1024);

    // Determinism: stamp the payload tree BEFORE cpio so the cpio header mtimes
    // are fixed, and the final flattened .pkg AFTER xar. The payload is ALWAYS
    // stamped — to the configured mod_timestamp when set, otherwise to a fixed
    // fallback epoch — so the cpio (and thus the .pkg) is byte-reproducible
    // even with no mod_timestamp; the same epoch is reused below for the xar
    // TOC so every time field in the archive agrees.
    let parsed_mtime = mod_timestamp
        .map(anodizer_core::util::parse_mod_timestamp)
        .transpose()?;
    let stamp_epoch = parsed_mtime
        .map(system_time_to_epoch)
        .unwrap_or(PKG_FALLBACK_EPOCH);
    let stamp_time =
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(stamp_epoch.max(0) as u64);
    apply_mtime_recursive(&pkgroot, stamp_time)?;

    // Payload: cpio(odc) of the pkgroot, inode/dev-zeroed and gzipped in-process.
    cpio_gzip_archive(&pkgroot, &component.join("Payload"), log)?;

    // Bom: mkbom of the pkgroot. `-u 0 -g 0` forces root ownership so the Bom's
    // encoded uid/gid match the `-R 0:0` Payload and native pkgbuild, instead of
    // leaking the runner's per-user ids.
    let bom_out = component.join("Bom");
    let output = Command::new("mkbom")
        .arg("-u")
        .arg("0")
        .arg("-g")
        .arg("0")
        .arg(&pkgroot)
        .arg(&bom_out)
        .output()
        .with_context(|| format!("execute mkbom for {}", pkgroot.display()))?;
    log.check_output(output, "mkbom")?;

    // Scripts: cpio.gz of the scripts dir, referenced from PackageInfo. pkgbuild
    // packages preinstall/postinstall this way for Installer to extract+run.
    let mut scripts_attr = String::new();
    if let Some(scripts_dir) = scripts {
        let sp = std::path::Path::new(scripts_dir);
        if sp.is_dir() {
            if let Some(mtime) = parsed_mtime {
                apply_mtime_recursive(sp, mtime)?;
            }
            cpio_gzip_archive(sp, &component.join("Scripts"), log)?;
            scripts_attr =
                "    <scripts>\n      <postinstall file=\"./postinstall\"/>\n    </scripts>\n"
                    .to_string();
        }
    }

    // PackageInfo: install-location "/" because pkgroot already encodes the full
    // path (flat-package default). min_os_version, when set, is recorded on an
    // os-version element so it is not lost.
    let min_os_xml = min_os_version
        .map(|v| format!("    <os-version min=\"{}\"/>\n", xml_escape(v)))
        .unwrap_or_default();
    let package_info = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <pkg-info format-version=\"2\" identifier=\"{id}\" version=\"{ver}\" \
         install-location=\"/\" auth=\"root\">\n\
         {min_os}\
         {scripts}\
         \x20   <payload installKBytes=\"{kb}\" numberOfFiles=\"{nf}\"/>\n\
         \x20   <bundle-version/>\n\
         </pkg-info>\n",
        id = xml_escape(identifier),
        ver = xml_escape(version),
        min_os = min_os_xml,
        scripts = scripts_attr,
        kb = install_kbytes,
        nf = num_files,
    );
    fs::write(component.join("PackageInfo"), package_info)
        .with_context(|| "write PackageInfo".to_string())?;

    // Distribution: minimal installer-gui-script referencing base.pkg.
    let distribution = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <installer-gui-script minSpecVersion=\"1\">\n\
         \x20   <title>{title}</title>\n\
         \x20   <choices-outline>\n\
         \x20       <line choice=\"default\"/>\n\
         \x20   </choices-outline>\n\
         \x20   <choice id=\"default\" title=\"{title}\">\n\
         \x20       <pkg-ref id=\"{id}\"/>\n\
         \x20   </choice>\n\
         \x20   <pkg-ref id=\"{id}\" version=\"{ver}\" onConclusion=\"none\">base.pkg</pkg-ref>\n\
         </installer-gui-script>\n",
        title = xml_escape(identifier),
        id = xml_escape(identifier),
        ver = xml_escape(version),
    );
    fs::write(flatdir.join("Distribution"), distribution)
        .with_context(|| "write Distribution".to_string())?;

    if let Some(parent) = pkg_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create pkg output dir {}", parent.display()))?;
    }

    // Flatten: xar --compression none so the only compression is the inner
    // gzip'd Payload (deterministic; xar's own gzip stream is not).
    let pkg_abs = if pkg_path.is_absolute() {
        pkg_path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve cwd for pkg output path")?
            .join(pkg_path)
    };
    let output = Command::new("xar")
        .arg("--compression")
        .arg("none")
        .arg("-cf")
        .arg(&pkg_abs)
        .arg("Distribution")
        .arg("base.pkg")
        .current_dir(&flatdir)
        .output()
        .with_context(|| format!("execute xar for {}", pkg_abs.display()))?;
    log.check_output(output, "xar")?;

    // The xar TOC embeds wall-clock per-file ctime/mtime/atime, the live
    // inode, and an archive-level creation-time that msitools' xar fills from
    // `now` and does NOT derive from SOURCE_DATE_EPOCH — so two builds a second
    // apart produce different `.pkg` bytes despite an otherwise-pinned payload.
    // Rewrite those fields to the same epoch the payload was stamped with and
    // re-seal the archive so the output is byte-reproducible.
    normalize_xar_toc(&pkg_abs, stamp_epoch).with_context(|| {
        format!(
            "normalize xar TOC for reproducibility: {}",
            pkg_abs.display()
        )
    })?;

    anodizer_core::util::set_file_mtime(&pkg_abs, stamp_time)?;

    Ok(())
}

/// Fixed epoch (1 second past the Unix epoch) stamped into the xar TOC time
/// fields when no `mod_timestamp` is configured. Non-zero so the rendered
/// `1970-01-01T00:00:01Z` is unambiguous in a manifest.
const PKG_FALLBACK_EPOCH: i64 = 1;

/// Convert a [`std::time::SystemTime`] to whole Unix epoch seconds (floored;
/// pre-epoch times clamp to 0).
fn system_time_to_epoch(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Rewrite the time and inode fields of a flat `.pkg`'s xar table-of-contents
/// to `epoch_secs` and re-seal the archive, making the output byte-stable
/// across builds.
///
/// xar layout: a 28-byte big-endian header (`magic="xar!"`, `header_size`,
/// `version`, `toc_length_compressed`, `toc_length_uncompressed`,
/// `cksum_alg`), then the zlib-compressed TOC, then the heap. The heap opens
/// with the TOC checksum (SHA-1, the digest of the *compressed* TOC, length
/// per the TOC's own `<checksum>` element) followed by the file data blobs,
/// whose `<data><offset>` values are heap-relative and so unaffected by a
/// change in compressed-TOC length.
///
/// The rewrite: decompress the TOC, replace every `<ctime>/<mtime>/<atime>`,
/// the archive `<creation-time>`, and every `<inode>` with the fixed values,
/// recompress deterministically, recompute the SHA-1 over the new compressed
/// TOC, and reassemble `header' + compressed_toc' + checksum' + heap_body`.
/// Because both the compressed TOC and its checksum derive from the same
/// normalized input, repeated builds yield identical bytes. The checksum
/// algorithm is assumed SHA-1 (`cksum_alg == 1`), which is what msitools' xar
/// and Apple's xar emit for `--compression none`; an unexpected algorithm is
/// left untouched and reported so the claim never silently overstates.
fn normalize_xar_toc(path: &std::path::Path, epoch_secs: i64) -> Result<()> {
    use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
    use std::io::{Read as _, Write as _};

    let data = fs::read(path).with_context(|| format!("read pkg {}", path.display()))?;
    if data.len() < 28 || &data[0..4] != b"xar!" {
        anyhow::bail!("not a xar archive (bad magic): {}", path.display());
    }
    let header_size = u16::from_be_bytes([data[4], data[5]]) as usize;
    let toc_len_comp = u64::from_be_bytes(data[8..16].try_into().unwrap()) as usize;
    let cksum_alg = u32::from_be_bytes(data[24..28].try_into().unwrap());
    // SHA-1 is the only algorithm anodizer's flat-pkg path produces; refuse to
    // touch anything else rather than reseal with a wrong-width/garbage digest.
    if cksum_alg != 1 {
        anyhow::bail!(
            "unexpected xar checksum algorithm {cksum_alg} (expected 1=SHA-1); \
             refusing to reseal {}",
            path.display()
        );
    }
    if header_size + toc_len_comp > data.len() {
        anyhow::bail!("truncated xar archive: {}", path.display());
    }
    let comp_toc = &data[header_size..header_size + toc_len_comp];
    let heap = &data[header_size + toc_len_comp..];

    // The leading bytes of the heap are the stored TOC checksum (SHA-1 = 20B);
    // the rest is the file-data body, copied through verbatim.
    const SHA1_LEN: usize = 20;
    if heap.len() < SHA1_LEN {
        anyhow::bail!("xar heap too small for checksum: {}", path.display());
    }
    let heap_body = &heap[SHA1_LEN..];

    let mut toc = Vec::new();
    ZlibDecoder::new(comp_toc)
        .read_to_end(&mut toc)
        .with_context(|| "decompress xar TOC".to_string())?;

    let stamp = epoch_to_iso8601(epoch_secs);
    let toc = rewrite_toc_fields(&toc, &stamp);

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder
        .write_all(&toc)
        .with_context(|| "recompress xar TOC".to_string())?;
    let new_comp = encoder
        .finish()
        .with_context(|| "finalize xar TOC".to_string())?;
    let new_cksum = sha1_digest(&new_comp);

    let mut out = Vec::with_capacity(header_size + new_comp.len() + SHA1_LEN + heap_body.len());
    out.extend_from_slice(&data[0..4]); // magic
    out.extend_from_slice(&data[4..6]); // header_size
    out.extend_from_slice(&data[6..8]); // version
    out.extend_from_slice(&(new_comp.len() as u64).to_be_bytes()); // toc_length_compressed
    out.extend_from_slice(&(toc.len() as u64).to_be_bytes()); // toc_length_uncompressed
    out.extend_from_slice(&data[24..header_size]); // cksum_alg + any header tail
    out.extend_from_slice(&new_comp);
    out.extend_from_slice(&new_cksum);
    out.extend_from_slice(heap_body);

    fs::write(path, &out).with_context(|| format!("rewrite pkg {}", path.display()))?;
    Ok(())
}

/// Replace the per-file `<ctime>/<mtime>/<atime>`, archive `<creation-time>`,
/// and `<inode>` elements in a decompressed xar TOC with fixed values.
///
/// `<ctime>/<mtime>/<atime>` use a `Z`-suffixed RFC-3339 stamp; xar's
/// archive-level `<creation-time>` is the same instant without the `Z`. Inodes
/// are pinned to `0` (matching pkgbuild's identity-stripped output).
fn rewrite_toc_fields(toc: &[u8], stamp: &str) -> Vec<u8> {
    let toc = String::from_utf8_lossy(toc);
    let mut s = toc.into_owned();
    for tag in ["ctime", "mtime", "atime"] {
        s = replace_element(&s, tag, &format!("{stamp}Z"));
    }
    s = replace_element(&s, "creation-time", stamp);
    s = replace_element(&s, "inode", "0");
    s.into_bytes()
}

/// Replace the text content of every `<tag>…</tag>` occurrence with `value`.
///
/// A linear scan (not regex) so the function carries no dependency and cannot
/// backtrack; xar TOC tags are simple non-nested leaf elements.
fn replace_element(s: &str, tag: &str, value: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(&open) {
        let after_open = start + open.len();
        if let Some(rel_end) = rest[after_open..].find(&close) {
            out.push_str(&rest[..after_open]);
            out.push_str(value);
            let end = after_open + rel_end;
            out.push_str(&close);
            rest = &rest[end + close.len()..];
        } else {
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Format whole Unix epoch seconds as `YYYY-MM-DDTHH:MM:SS` (UTC, no `Z`).
fn epoch_to_iso8601(epoch_secs: i64) -> String {
    // Civil-from-days (Howard Hinnant's algorithm), avoiding a chrono dep.
    let secs = epoch_secs.rem_euclid(86_400);
    let days = (epoch_secs - secs) / 86_400;
    let (hour, min, sec) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}")
}

/// Compute the SHA-1 digest of `data` (the algorithm xar records for its TOC
/// checksum). Self-contained to avoid pulling a crypto crate for a 20-byte
/// non-security digest.
fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let ml = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// PkgStage
// ---------------------------------------------------------------------------

/// Default output filename template (no version component).
///
/// The `.pkg` extension is auto-appended (case-insensitively) after rendering
/// when the resolved name does not already end in `.pkg`, so the default emits
/// `<ProjectName>_<Arch>.pkg`. An extensionless installer is not recognized by
/// macOS Installer.app and breaks the homebrew-cask pkg stanza + checksum
/// naming. A user-supplied `name:` ending in `.pkg` is used verbatim (not
/// doubled).
const DEFAULT_NAME_TEMPLATE: &str = "{{ ProjectName }}_{{ Arch }}";

/// Rendered per-binary field values resolved from a [`PkgConfig`].
///
/// Produced by [`render_pkg_fields`] for one (config, target) pairing
/// after `Os`/`Arch`/`Target` template vars are set, so every template
/// expansion sees the binary's effective target triple.
pub struct RenderedPkgFields {
    pub identifier: String,
    pub install_location: String,
    pub scripts: Option<String>,
    pub mod_timestamp: Option<String>,
}

/// Render the four template-bearing `PkgConfig` fields against `ctx`.
///
/// `identifier_template` must already be unwrapped from
/// `pkg_cfg.identifier` (which is required at validation time); the rest
/// are resolved against `pkg_cfg`. `crate_name` and `target` are used
/// only to build error context.
pub fn render_pkg_fields(
    ctx: &mut Context,
    pkg_cfg: &anodizer_core::config::PkgConfig,
    identifier_template: &str,
    crate_name: &str,
    target: Option<&str>,
) -> Result<RenderedPkgFields> {
    let identifier = ctx.render_template(identifier_template).with_context(|| {
        format!(
            "pkg: render identifier template for crate {} target {:?}",
            crate_name, target
        )
    })?;

    let install_location_raw = pkg_cfg
        .install_location
        .as_deref()
        .unwrap_or("/usr/local/bin");
    let install_location = ctx.render_template(install_location_raw).with_context(|| {
        format!(
            "pkg: render install_location template for crate {} target {:?}",
            crate_name, target
        )
    })?;

    let scripts = pkg_cfg
        .scripts
        .as_deref()
        .map(|s| {
            ctx.render_template(s).with_context(|| {
                format!(
                    "pkg: render scripts template for crate {} target {:?}",
                    crate_name, target
                )
            })
        })
        .transpose()?;

    let mod_timestamp = pkg_cfg
        .mod_timestamp
        .as_deref()
        .map(|ts| {
            ctx.render_template(ts).with_context(|| {
                format!(
                    "pkg: render mod_timestamp template for crate {} target {:?}",
                    crate_name, target
                )
            })
        })
        .transpose()?;

    Ok(RenderedPkgFields {
        identifier,
        install_location,
        scripts,
        mod_timestamp,
    })
}

pub struct PkgStage;

impl Stage for PkgStage {
    fn name(&self) -> &str {
        "pkg"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("pkg");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have pkg config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.pkgs.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        // Resolve version from template vars
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());

        // In workspace per-crate mode the same pipeline run produces a pkg for
        // each crate. Rebinding `ProjectName` to the current crate's name
        // (mirroring the archive stage) keeps default name templates like
        // `{{ ProjectName }}_{{ Arch }}` distinct per crate so two crates'
        // installers don't render the same filename and clobber each other.
        // Restored after the loop.
        let multi_crate = crates.len() > 1;
        let original_project_name = ctx
            .template_vars()
            .get("ProjectName")
            .cloned()
            .unwrap_or_else(|| ctx.config.project_name.clone());

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut archive_paths_to_remove: Vec<PathBuf> = Vec::new();

        // Capture the loop result rather than `?`-ing inside it: a per-crate
        // failure must still restore the rebound `ProjectName` below before
        // propagating, so the workspace value never leaks past this stage.
        let loop_result: Result<()> = (|| {
            for krate in &crates {
                let Some(pkg_configs) = krate.pkgs.as_ref() else {
                    continue;
                };
                if multi_crate {
                    ctx.template_vars_mut().set("ProjectName", &krate.name);
                }

                // Collect macOS binary artifacts for this crate
                let darwin_binaries: Vec<_> = ctx
                    .artifacts
                    .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                    .into_iter()
                    .filter(|b| {
                        b.target
                            .as_deref()
                            .map(anodizer_core::target::is_darwin)
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect();

                for pkg_cfg in pkg_configs {
                    let pkg_id_for_log = pkg_cfg.id.as_deref().unwrap_or("default").to_string();

                    // `pkg.if`: template-conditional skip (opt-in).
                    // Render error => hard bail (W1 avoidance).
                    let proceed = anodizer_core::config::evaluate_if_condition(
                        pkg_cfg.if_condition.as_deref(),
                        &format!("pkg config '{}' for crate '{}'", pkg_id_for_log, krate.name),
                        |t| ctx.render_template(t),
                    )?;
                    if !proceed {
                        log.status(&format!(
                            "skipped pkg config '{}' for crate {} — `if` condition evaluated falsy",
                            pkg_id_for_log, krate.name
                        ));
                        continue;
                    }

                    // Skip configs marked skip:
                    if let Some(ref d) = pkg_cfg.skip {
                        let off = d
                            .try_evaluates_to_true(|s| ctx.render_template(s))
                            .with_context(|| {
                                format!("pkg: render skip template for crate {}", krate.name)
                            })?;
                        if off {
                            log.status(&format!("pkg config skipped for crate {}", krate.name));
                            continue;
                        }
                    }

                    // Validate `use` field
                    let use_mode = pkg_cfg.use_.as_deref().unwrap_or("binary");
                    if use_mode != "binary" && use_mode != "appbundle" {
                        anyhow::bail!(
                            "pkg: invalid `use` value '{}' for crate '{}'; expected 'binary' or 'appbundle'",
                            use_mode,
                            krate.name
                        );
                    }

                    // Collect source artifacts depending on `use` mode
                    let source_artifacts: Vec<_> = if use_mode == "appbundle" {
                        // Collect Installer artifacts with format=appbundle for this crate
                        ctx.artifacts
                            .by_kind_and_crate(ArtifactKind::Installer, &krate.name)
                            .into_iter()
                            .filter(|a| {
                                a.metadata
                                    .get("format")
                                    .map(|f| f == "appbundle")
                                    .unwrap_or(false)
                            })
                            .cloned()
                            .collect()
                    } else {
                        darwin_binaries.clone()
                    };

                    // Filter by build IDs if specified
                    let mut filtered = source_artifacts.clone();
                    if let Some(ref filter_ids) = pkg_cfg.ids
                        && !filter_ids.is_empty()
                    {
                        filtered.retain(|b| {
                            b.metadata
                                .get("id")
                                .map(|id| filter_ids.contains(id))
                                .unwrap_or(false)
                                || b.metadata
                                    .get("name")
                                    .map(|n| filter_ids.contains(n))
                                    .unwrap_or(false)
                        });
                    }

                    // Warn and skip if no source artifacts found
                    if filtered.is_empty() && source_artifacts.is_empty() {
                        if use_mode == "appbundle" {
                            log.warn(&format!(
                                "skipped PKG generation for crate '{}' — no appbundle artifacts \
                             found (expected Installer artifacts with format=appbundle)",
                                krate.name
                            ));
                        } else {
                            log.warn(&format!(
                                "skipped PKG generation for crate '{}' — no macOS binary \
                             artifacts found (expected binaries targeting darwin/apple)",
                                krate.name
                            ));
                        }
                        continue;
                    }
                    if filtered.is_empty() {
                        log.warn(&format!(
                            "skipped pkg for crate '{}' — ids filter {:?} matched no artifacts",
                            krate.name, pkg_cfg.ids
                        ));
                        continue;
                    }

                    let effective_binaries: Vec<(Option<String>, PathBuf)> = filtered
                        .iter()
                        .map(|b| (b.target.clone(), b.path.clone()))
                        .collect();

                    // Validate identifier is present (template render happens inside the
                    // per-binary loop below so Os/Arch vars are set first).
                    let identifier_template = pkg_cfg.identifier.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "pkg: missing required `identifier` for crate `{}`. \
                         Set a reverse-domain identifier (e.g. com.example.myapp)",
                            krate.name
                        )
                    })?;

                    // Resolve the build path once per config entry before the
                    // per-binary loop so the error surfaces early naming both
                    // options. `pkgbuild` (macOS) is preferred; otherwise the
                    // Linux flat-package toolchain assembles the identical XAR
                    // layout by hand. Dry-run never requires a tool.
                    let builder = if dry_run {
                        PkgBuilder::Pkgbuild
                    } else {
                        resolve_pkg_builder(anodizer_core::util::find_binary)
                            .map_err(anyhow::Error::msg)?
                    };

                    // One .pkg is produced per binary — pkg installers are single-binary
                    // by design. Unlike DMG (which groups multiple binaries into one
                    // container image), each pkg wraps exactly one payload binary so that
                    // Homebrew formula installers and macOS Installer.app each target a
                    // discrete, independently versionable package. Multi-binary crates
                    // therefore emit N packages per target triple.
                    for (target, binary_path) in &effective_binaries {
                        // Derive Os/Arch from the target triple for template rendering
                        let (os, arch) = target
                            .as_deref()
                            .map(anodizer_core::target::map_target)
                            .unwrap_or_else(|| ("darwin".to_string(), "amd64".to_string()));

                        // Set Os/Arch/Target in template vars for name template rendering
                        ctx.template_vars_mut().set("Os", &os);
                        ctx.template_vars_mut().set("Arch", &arch);
                        ctx.template_vars_mut()
                            .set("Target", target.as_deref().unwrap_or(""));

                        let rendered = render_pkg_fields(
                            ctx,
                            pkg_cfg,
                            identifier_template,
                            &krate.name,
                            target.as_deref(),
                        )?;
                        let identifier = rendered.identifier;
                        let install_location = rendered.install_location;
                        let scripts_rendered = rendered.scripts;
                        let mod_timestamp_rendered = rendered.mod_timestamp;

                        // Determine output filename
                        let name_template =
                            pkg_cfg.name.as_deref().unwrap_or(DEFAULT_NAME_TEMPLATE);

                        let pkg_filename =
                            ctx.render_template(name_template).with_context(|| {
                                format!(
                                    "pkg: render name template for crate {} target {:?}",
                                    krate.name, target
                                )
                            })?;

                        // Ensure the filename ends with .pkg (case-insensitive). An
                        // extensionless installer is not recognized by macOS
                        // Installer.app and breaks the homebrew-cask pkg stanza +
                        // checksum naming; a user-supplied `name` already ending in
                        // `.pkg` is not doubled. Mirrors stage-dmg's `.dmg` append.
                        let pkg_filename = if pkg_filename.to_ascii_lowercase().ends_with(".pkg") {
                            pkg_filename
                        } else {
                            format!("{pkg_filename}.pkg")
                        };

                        let output_dir = dist.join("macos");
                        let pkg_path = output_dir.join(&pkg_filename);

                        if dry_run {
                            log.status(&format!(
                                "(dry-run) would run: pkgbuild --identifier {identifier} \
                             --version {version} for crate {} target {:?}",
                                krate.name, target
                            ));

                            new_artifacts.push(Artifact {
                                kind: ArtifactKind::MacOsPackage,
                                name: String::new(),
                                path: pkg_path,
                                target: target.clone(),
                                crate_name: krate.name.clone(),
                                metadata: {
                                    let mut m = HashMap::from([(
                                        "identifier".to_string(),
                                        identifier.to_string(),
                                    )]);
                                    if let Some(id) = &pkg_cfg.id {
                                        m.insert("id".to_string(), id.clone());
                                    }
                                    m
                                },
                                size: None,
                            });

                            // Track archives to remove if replace is true
                            archive_paths_to_remove.extend(
                                anodizer_core::util::collect_if_replace(
                                    pkg_cfg.replace,
                                    &ctx.artifacts,
                                    &krate.name,
                                    target.as_deref(),
                                ),
                            );

                            continue;
                        }

                        // Live mode: create staging directory and copy binary into it
                        let staging_tmp =
                            tempfile::tempdir().context("create temp staging dir for pkg")?;
                        let staging_dir = staging_tmp.path();

                        // Copy the binary into the staging directory
                        let binary_name = binary_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(&krate.name);
                        let staged_binary = staging_dir.join(binary_name);
                        // In appbundle mode `binary_path` is a `.app` directory;
                        // fs::copy rejects directories, so recurse (symlink-safe,
                        // preserving the bundle's framework version links).
                        if binary_path.is_dir() {
                            anodizer_core::util::copy_dir_tree(binary_path, &staged_binary)
                                .with_context(|| {
                                    format!(
                                        "pkg: copy app bundle {} to staging dir {}",
                                        binary_path.display(),
                                        staging_dir.display()
                                    )
                                })?;
                        } else {
                            fs::copy(binary_path, &staged_binary).with_context(|| {
                                format!(
                                    "pkg: copy binary {} to staging dir {}",
                                    binary_path.display(),
                                    staging_dir.display()
                                )
                            })?;
                        }

                        // Copy extra files into the staging directory
                        if let Some(extra_files) = &pkg_cfg.extra_files {
                            for spec in extra_files {
                                let glob_pattern = spec.glob();
                                for entry in glob::glob(glob_pattern).with_context(|| {
                                    format!("pkg: invalid extra_files glob '{}'", glob_pattern)
                                })? {
                                    let src = entry.with_context(|| {
                                        format!(
                                            "pkg: error reading glob match for '{}'",
                                            glob_pattern
                                        )
                                    })?;
                                    let dst_name = spec
                                        .name_template()
                                        .map(|s| s.to_string())
                                        .or_else(|| {
                                            src.file_name()
                                                .and_then(|n| n.to_str())
                                                .map(|s| s.to_string())
                                        })
                                        .unwrap_or_else(|| "extra".to_string());
                                    let dst = staging_dir.join(&dst_name);
                                    fs::copy(&src, &dst).with_context(|| {
                                        format!(
                                            "pkg: copy extra file {} to staging dir",
                                            src.display()
                                        )
                                    })?;
                                }
                            }
                        }

                        // Render and copy templated_extra_files into the staging directory
                        if let Some(ref tpl_specs) = pkg_cfg.templated_extra_files
                            && !tpl_specs.is_empty()
                        {
                            anodizer_core::templated_files::process_templated_extra_files(
                                tpl_specs,
                                ctx,
                                staging_dir,
                                "pkg",
                            )?;
                        }

                        // Apply mod_timestamp if set. Templates were already expanded
                        // upstream via render_pkg_fields, so values like
                        // `{{ CommitTimestamp }}` reach parse_mod_timestamp as a
                        // valid RFC3339 string rather than the literal template.
                        if let Some(ts) = &mod_timestamp_rendered {
                            anodizer_core::util::apply_mod_timestamp(staging_dir, ts, &log)?;
                        }

                        // Ensure output directory exists
                        fs::create_dir_all(&output_dir).with_context(|| {
                            format!("create pkg output dir: {}", output_dir.display())
                        })?;

                        match builder {
                            PkgBuilder::Pkgbuild => {
                                let cmd_args = pkgbuild_command(
                                    &staging_dir.to_string_lossy(),
                                    &identifier,
                                    &version,
                                    &install_location,
                                    scripts_rendered.as_deref(),
                                    pkg_cfg.min_os_version.as_deref(),
                                    &pkg_path.to_string_lossy(),
                                );

                                log.verbose(&format!("running {}", cmd_args.join(" ")));

                                let output = Command::new(&cmd_args[0])
                                    .args(&cmd_args[1..])
                                    .output()
                                    .with_context(|| {
                                        format!(
                                            "execute pkgbuild for crate {} target {:?}",
                                            krate.name, target
                                        )
                                    })?;
                                log.check_output(output, "pkgbuild")?;

                                // Stamp the output mtime for native-vs-Linux
                                // parity (the Linux path stamps its .pkg too).
                                // The mtime is not in the archive bytes, so
                                // checksums are unaffected.
                                if let Some(ts) = &mod_timestamp_rendered {
                                    let mtime = anodizer_core::util::parse_mod_timestamp(ts)?;
                                    anodizer_core::util::set_file_mtime(&pkg_path, mtime)?;
                                }
                            }
                            PkgBuilder::Linux => {
                                log.status(&format!(
                                    "assembling flat .pkg (Linux xar/mkbom/cpio/gzip) for crate {} target {:?}",
                                    krate.name, target
                                ));
                                build_flat_pkg_linux(
                                    staging_dir,
                                    &identifier,
                                    &version,
                                    &install_location,
                                    scripts_rendered.as_deref(),
                                    pkg_cfg.min_os_version.as_deref(),
                                    mod_timestamp_rendered.as_deref(),
                                    &pkg_path,
                                    &log,
                                )?;
                            }
                        }

                        log.status(&format!(
                            "built pkg {}",
                            pkg_path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| pkg_path.to_string_lossy().into_owned())
                        ));

                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::MacOsPackage,
                            name: String::new(),
                            path: pkg_path,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            metadata: {
                                let mut m = HashMap::from([(
                                    "identifier".to_string(),
                                    identifier.to_string(),
                                )]);
                                if let Some(id) = &pkg_cfg.id {
                                    m.insert("id".to_string(), id.clone());
                                }
                                m
                            },
                            size: None,
                        });

                        // Track archives to remove if replace is true
                        archive_paths_to_remove.extend(anodizer_core::util::collect_if_replace(
                            pkg_cfg.replace,
                            &ctx.artifacts,
                            &krate.name,
                            target.as_deref(),
                        ));
                    }
                }
            }
            Ok(())
        })();

        if multi_crate {
            ctx.template_vars_mut()
                .set("ProjectName", &original_project_name);
        }
        loop_result?;

        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());

        // Remove archive artifacts marked for replacement
        if !archive_paths_to_remove.is_empty() {
            ctx.artifacts.remove_by_paths(&archive_paths_to_remove);
        }

        // Register new PKG artifacts
        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Environment requirements for the pkg stage: either `pkgbuild` (macOS) or the
/// Linux flat-package toolchain, when any active `pkgs:` entry exists and the
/// configured build targets include macOS (the stage only packages darwin
/// binaries).
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    if !anodizer_core::env_preflight::configured_build_targets(ctx)
        .iter()
        .any(|t| anodizer_core::target::is_darwin(t))
    {
        return Vec::new();
    }
    let configured = anodizer_core::env_preflight::crate_universe(&ctx.config)
        .into_iter()
        .flat_map(|c| c.pkgs.iter().flatten())
        .any(|cfg| {
            !anodizer_core::env_preflight::entry_inactive(
                ctx,
                cfg.skip.as_ref(),
                None,
                cfg.if_condition.as_deref(),
            )
        });
    if !configured {
        return Vec::new();
    }
    // `xar` is the sentinel for the Linux flat-package path: ToolAnyOf is
    // any-of and cannot express "all three of xar+mkbom+cpio together", so
    // preflight surfaces "pkgbuild OR xar"; the build-time resolution in
    // `resolve_pkg_builder` still enforces the full three-tool group and bails
    // with the precise message when only a partial Linux toolchain is present.
    vec![anodizer_core::EnvRequirement::ToolAnyOf {
        names: vec!["pkgbuild".to_string(), "xar".to_string()],
    }]
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{Config, CrateConfig, ExtraFileSpec, PkgConfig, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    use tempfile::TempDir;

    // -- pkgbuild_command tests --

    #[test]
    fn test_pkgbuild_command_basic() {
        let cmd = pkgbuild_command(
            "/tmp/staging",
            "com.example.myapp",
            "1.0.0",
            "/usr/local/bin",
            None,
            None,
            "/tmp/output/myapp.pkg",
        );
        assert_eq!(
            cmd,
            vec![
                "pkgbuild",
                "--root",
                "/tmp/staging",
                "--identifier",
                "com.example.myapp",
                "--version",
                "1.0.0",
                "--install-location",
                "/usr/local/bin",
                "/tmp/output/myapp.pkg",
            ]
        );
    }

    #[test]
    fn test_pkgbuild_command_with_scripts() {
        let cmd = pkgbuild_command(
            "/tmp/staging",
            "com.example.myapp",
            "2.0.0",
            "/usr/local/bin",
            Some("/path/to/scripts"),
            None,
            "/tmp/output/myapp.pkg",
        );
        assert_eq!(
            cmd,
            vec![
                "pkgbuild",
                "--root",
                "/tmp/staging",
                "--identifier",
                "com.example.myapp",
                "--version",
                "2.0.0",
                "--install-location",
                "/usr/local/bin",
                "--scripts",
                "/path/to/scripts",
                "/tmp/output/myapp.pkg",
            ]
        );
    }

    #[test]
    fn test_pkgbuild_command_custom_install_location() {
        let cmd = pkgbuild_command(
            "/tmp/staging",
            "com.example.myapp",
            "1.0.0",
            "/opt/myapp/bin",
            None,
            None,
            "/tmp/output/myapp.pkg",
        );
        assert_eq!(
            cmd,
            vec![
                "pkgbuild",
                "--root",
                "/tmp/staging",
                "--identifier",
                "com.example.myapp",
                "--version",
                "1.0.0",
                "--install-location",
                "/opt/myapp/bin",
                "/tmp/output/myapp.pkg",
            ]
        );
    }

    // -- tool resolution tests --

    #[test]
    fn test_resolve_prefers_pkgbuild() {
        let r = resolve_pkg_builder(|t| t == "pkgbuild");
        assert_eq!(r, Ok(PkgBuilder::Pkgbuild));
    }

    #[test]
    fn test_resolve_linux_when_full_toolchain() {
        let r = resolve_pkg_builder(|t| LINUX_PKG_TOOLS.contains(&t));
        assert_eq!(r, Ok(PkgBuilder::Linux));
    }

    #[test]
    fn test_resolve_bail_names_both_options() {
        // Partial Linux toolchain (missing cpio) and no pkgbuild => error.
        let err = resolve_pkg_builder(|t| t == "xar" || t == "mkbom").unwrap_err();
        assert!(
            err.contains("pkgbuild"),
            "message must name pkgbuild: {err}"
        );
        assert!(
            err.contains("xar"),
            "message must name the Linux toolchain: {err}"
        );
        assert!(err.contains("mkbom"), "message must name mkbom: {err}");
        assert!(err.contains("cpio"), "message must name cpio: {err}");
    }

    // -- Linux flat-package builder --

    // -- reproducibility helpers --

    #[test]
    fn test_sha1_digest_known_vectors() {
        assert_eq!(
            sha1_digest(b""),
            hex_to_bytes("da39a3ee5e6b4b0d3255bfef95601890afd80709")
        );
        assert_eq!(
            sha1_digest(b"abc"),
            hex_to_bytes("a9993e364706816aba3e25717850c26c9cd0d89d")
        );
        assert_eq!(
            sha1_digest(b"The quick brown fox jumps over the lazy dog"),
            hex_to_bytes("2fd4e1c67a2d28fced849ee1bb76e7391b93eb12")
        );
    }

    fn hex_to_bytes(h: &str) -> [u8; 20] {
        let mut out = [0u8; 20];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn test_epoch_to_iso8601() {
        assert_eq!(epoch_to_iso8601(0), "1970-01-01T00:00:00");
        assert_eq!(epoch_to_iso8601(1), "1970-01-01T00:00:01");
        // 1700000000 = 2023-11-14T22:13:20Z
        assert_eq!(epoch_to_iso8601(1_700_000_000), "2023-11-14T22:13:20");
        // Leap day
        assert_eq!(epoch_to_iso8601(1_582_934_400), "2020-02-29T00:00:00");
    }

    #[test]
    fn test_rewrite_toc_fields_normalizes_all_time_and_inode() {
        let toc = b"<file><ctime>2026-01-01T00:00:00Z</ctime>\
                    <mtime>2026-01-01T00:00:00Z</mtime>\
                    <atime>2026-01-01T00:00:00Z</atime>\
                    <inode>1656875</inode></file>\
                    <creation-time>2026-01-01T00:00:00</creation-time>";
        let out = String::from_utf8(rewrite_toc_fields(toc, "1970-01-01T00:00:01")).unwrap();
        assert!(out.contains("<ctime>1970-01-01T00:00:01Z</ctime>"));
        assert!(out.contains("<mtime>1970-01-01T00:00:01Z</mtime>"));
        assert!(out.contains("<atime>1970-01-01T00:00:01Z</atime>"));
        assert!(out.contains("<inode>0</inode>"));
        assert!(out.contains("<creation-time>1970-01-01T00:00:01</creation-time>"));
        assert!(!out.contains("2026"));
    }

    #[test]
    fn test_normalize_odc_cpio_zeroes_dev_ino_and_is_idempotent() {
        if !anodizer_core::util::find_binary("cpio") || !anodizer_core::util::find_binary("sh") {
            eprintln!("cpio absent; test skipped hermetically");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        fs::write(dir.path().join("b.txt"), b"world").unwrap();
        let raw = Command::new("sh")
            .arg("-c")
            .arg("find . | LC_ALL=C sort | cpio -o --format odc -R 0:0 2>/dev/null")
            .current_dir(dir.path())
            .output()
            .unwrap()
            .stdout;
        let n1 = normalize_odc_cpio(&raw);
        // Every odc header's dev (6..12) and ino (12..18) must be zeroed.
        let mut i = 0;
        while i + 76 <= n1.len() && &n1[i..i + 6] == b"070707" {
            assert_eq!(&n1[i + 6..i + 12], b"000000", "dev not zeroed at {i}");
            assert_eq!(&n1[i + 12..i + 18], b"000000", "ino not zeroed at {i}");
            let namesize =
                usize::from_str_radix(std::str::from_utf8(&n1[i + 59..i + 65]).unwrap(), 8)
                    .unwrap();
            let filesize =
                usize::from_str_radix(std::str::from_utf8(&n1[i + 65..i + 76]).unwrap(), 8)
                    .unwrap();
            let name = &n1[i + 76..i + 76 + namesize];
            if name.starts_with(b"TRAILER!!!") {
                break;
            }
            i += 76 + namesize + filesize;
        }
        // Idempotent: re-normalizing yields the identical bytes.
        assert_eq!(normalize_odc_cpio(&n1), n1);
    }

    #[test]
    fn test_flat_pkg_is_byte_reproducible_across_time() {
        // The whole point of the fix: two builds whose wall-clock differs must
        // produce byte-identical `.pkg`. Builds the same staging twice with a
        // simulated time gap (a real 2nd build re-runs xar, which re-stamps the
        // TOC wall-clock — here we build twice back to back, which already
        // exercises distinct xar `creation-time`s on most hosts; the normalize
        // pass collapses them). Hermetic: skip-with-pass without the toolchain.
        // Linux-only: bomutils `mkbom -u` is rejected by Apple's homonym `mkbom`,
        // so a macOS host (which ships xar/mkbom/cpio under the same names) would
        // falsely satisfy the tool probe and then crash on the bomutils syntax —
        // this fallback path is never taken on macOS in production anyway.
        let have_tools = cfg!(target_os = "linux")
            && LINUX_PKG_TOOLS
                .iter()
                .all(|t| anodizer_core::util::find_binary(t))
            && anodizer_core::util::find_binary("sh");
        if !have_tools {
            eprintln!("Linux pkg toolchain absent; test skipped hermetically");
            return;
        }
        let log =
            anodizer_core::log::StageLogger::new("pkg", anodizer_core::log::Verbosity::Normal);
        let build = || -> Vec<u8> {
            let staging = TempDir::new().unwrap();
            fs::write(staging.path().join("myapp"), b"#!/bin/sh\necho hi\n").unwrap();
            let out = TempDir::new().unwrap();
            let pkg_path = out.path().join("myapp_arm64.pkg");
            build_flat_pkg_linux(
                staging.path(),
                "com.example.myapp",
                "1.2.3",
                "/usr/local/bin",
                None,
                Some("11.0"),
                None, // no mod_timestamp => fallback-epoch path
                &pkg_path,
                &log,
            )
            .unwrap();
            fs::read(&pkg_path).unwrap()
        };
        let a = build();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = build();
        assert_eq!(a, b, "flat .pkg must be byte-identical across builds");
    }

    #[test]
    fn test_build_flat_pkg_linux_emits_xar_layout() {
        // Hermetic: skip-with-pass if the Linux toolchain is absent. This box
        // has all of xar/mkbom/cpio, so the assertions below WILL execute here.
        // Linux-only: Apple's `mkbom` rejects the bomutils `-u` flag, so a macOS
        // host would falsely pass the probe and crash; the path is Linux-only.
        let have_tools = cfg!(target_os = "linux")
            && LINUX_PKG_TOOLS
                .iter()
                .all(|t| anodizer_core::util::find_binary(t))
            && anodizer_core::util::find_binary("sh");
        if !have_tools {
            eprintln!("Linux pkg toolchain absent; test skipped hermetically");
            return;
        }

        let staging = TempDir::new().unwrap();
        fs::write(staging.path().join("myapp"), b"#!/bin/sh\necho hi\n").unwrap();

        let scripts = TempDir::new().unwrap();
        fs::write(scripts.path().join("postinstall"), b"#!/bin/sh\nexit 0\n").unwrap();

        let out = TempDir::new().unwrap();
        let pkg_path = out.path().join("myapp_arm64.pkg");

        let log =
            anodizer_core::log::StageLogger::new("pkg", anodizer_core::log::Verbosity::Normal);
        build_flat_pkg_linux(
            staging.path(),
            "com.example.myapp",
            "1.2.3",
            "/usr/local/bin",
            Some(scripts.path().to_str().unwrap()),
            Some("11.0"),
            Some("1704067200"),
            &pkg_path,
            &log,
        )
        .expect("flat pkg build");

        assert!(pkg_path.exists(), "output .pkg must exist");

        let listing = Command::new("xar")
            .arg("-tf")
            .arg(&pkg_path)
            .output()
            .expect("xar -tf");
        assert!(listing.status.success(), "xar -tf must succeed");
        let toc = String::from_utf8_lossy(&listing.stdout);
        assert!(
            toc.contains("Distribution"),
            "TOC must list Distribution: {toc}"
        );
        assert!(
            toc.contains("base.pkg/Payload"),
            "TOC must list Payload: {toc}"
        );
        assert!(toc.contains("base.pkg/Bom"), "TOC must list Bom: {toc}");
        assert!(
            toc.contains("base.pkg/Scripts"),
            "TOC must list Scripts when scripts dir set: {toc}"
        );
    }

    // -- Stage no-op / skip tests --

    #[test]
    fn test_stage_skips_when_no_pkg_config() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = PkgStage;
        assert!(stage.run(&mut ctx).is_ok());
        // No artifacts should be registered
        assert!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty());
    }

    #[test]
    fn test_stage_skips_when_disabled() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add a darwin binary so the stage would otherwise process it
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        // No packages should be generated because the config is disabled
        assert!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty());
    }

    // -- Dry-run behavior tests --

    #[test]
    fn test_stage_dry_run_registers_artifacts() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register darwin binary artifacts
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp-x86"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 2, "should register one PKG per darwin binary");

        // Both should have correct kind and metadata
        for pkg in &pkgs {
            assert_eq!(pkg.kind, ArtifactKind::MacOsPackage);
            assert_eq!(pkg.crate_name, "myapp");
            assert_eq!(
                pkg.metadata.get("identifier"),
                Some(&"com.example.myapp".to_string())
            );
        }

        // Check targets are preserved
        let targets: Vec<Option<&str>> = pkgs.iter().map(|p| p.target.as_deref()).collect();
        assert!(targets.contains(&Some("aarch64-apple-darwin")));
        assert!(targets.contains(&Some("x86_64-apple-darwin")));
    }

    #[test]
    fn test_workspace_per_crate_distinct_filenames() {
        let tmp = TempDir::new().unwrap();

        // Two crates, both using the DEFAULT name template (no Version segment),
        // so ProjectName is the only distinguishing token. Without the per-crate
        // ProjectName rebind both render to `<project_name>_arm64.pkg` and clobber.
        let make_crate = |name: &str| CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![PkgConfig {
                identifier: Some("com.example.{{ ProjectName }}".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "workspace".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![make_crate("alpha"), make_crate("beta")];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        for crate_name in ["alpha", "beta"] {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from(format!("/build/{crate_name}")),
                target: Some("aarch64-apple-darwin".to_string()),
                crate_name: crate_name.to_string(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 2, "expected one PKG per crate");

        let filenames: Vec<String> = pkgs
            .iter()
            .map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(
            filenames.iter().any(|f| f.contains("alpha")),
            "no PKG filename contains crate name 'alpha': {filenames:?}"
        );
        assert!(
            filenames.iter().any(|f| f.contains("beta")),
            "no PKG filename contains crate name 'beta': {filenames:?}"
        );
        assert_ne!(
            filenames[0], filenames[1],
            "the two crates' PKGs must not share a filename (clobber): {filenames:?}"
        );

        assert_eq!(
            ctx.template_vars().get("ProjectName").map(String::as_str),
            Some("workspace"),
            "ProjectName not restored after per-crate rebind"
        );
    }

    #[test]
    fn test_project_name_restored_after_mid_loop_error() {
        // A per-crate render failure mid-loop must still restore the rebound
        // `ProjectName` before propagating, so the workspace value never leaks
        // out of the stage (the var is process-global on ctx).
        let tmp = TempDir::new().unwrap();

        let good_crate = CrateConfig {
            name: "alpha".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![PkgConfig {
                identifier: Some("com.example.alpha".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let bad_crate = CrateConfig {
            name: "beta".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![PkgConfig {
                identifier: Some("com.example.beta".to_string()),
                // Malformed template — unclosed tag forces a mid-loop render error.
                name: Some("{{ bad_template".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "workspace".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![good_crate, bad_crate];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        for crate_name in ["alpha", "beta"] {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from(format!("/build/{crate_name}")),
                target: Some("aarch64-apple-darwin".to_string()),
                crate_name: crate_name.to_string(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        let result = PkgStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "malformed name on a crate must fail the stage"
        );

        assert_eq!(
            ctx.template_vars().get("ProjectName").map(String::as_str),
            Some("workspace"),
            "ProjectName must be restored even when the loop errors mid-iteration"
        );
    }

    #[test]
    fn test_stage_dry_run_with_name_template() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            name: Some("{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}.pkg".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "2.5.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1);

        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            filename, "myapp_2.5.0_darwin_arm64.pkg",
            "name template should render with Os/Arch from target triple"
        );
    }

    #[test]
    fn test_stage_dry_run_replace_removes_archives() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            replace: Some(true),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add a darwin binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // Add darwin archive artifacts that should be removed
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/dist/myapp_darwin_arm64.tar.gz"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // Add a linux archive that should NOT be removed
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/dist/myapp_linux_amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        // PKG artifact should be registered
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1);

        // Darwin archive should be removed
        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1, "darwin archive should be removed");
        assert_eq!(
            archives[0].target.as_deref(),
            Some("x86_64-unknown-linux-gnu"),
            "only the linux archive should remain"
        );
    }

    // -- Error path tests --

    #[test]
    fn test_stage_errors_without_identifier() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: None, // missing required field
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add a darwin binary so the stage attempts to process the config
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let result = PkgStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "missing identifier should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("identifier"),
            "error should mention missing identifier, got: {err}"
        );
    }

    // -- Config parsing tests --

    #[test]
    fn test_config_parse_pkg() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pkgs = config.crates[0].pkgs.as_ref().unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].identifier.as_deref(), Some("com.example.test"));
        // All optional fields default to None
        assert!(pkgs[0].name.is_none());
        assert!(pkgs[0].install_location.is_none());
        assert!(pkgs[0].scripts.is_none());
        assert!(pkgs[0].replace.is_none());
        assert!(pkgs[0].skip.is_none());
    }

    #[test]
    fn test_config_parse_pkg_full() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - id: my-pkg
        ids:
          - build-darwin-arm64
          - build-darwin-amd64
        identifier: com.example.test
        name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}"
        install_location: /opt/test/bin
        scripts: ./scripts/pkg
        extra_files:
          - README.md
          - LICENSE
        replace: true
        mod_timestamp: "2024-01-01T00:00:00Z"
        skip: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pkgs = config.crates[0].pkgs.as_ref().unwrap();
        assert_eq!(pkgs.len(), 1);
        let p = &pkgs[0];
        assert_eq!(p.id.as_deref(), Some("my-pkg"));
        assert_eq!(
            p.ids.as_ref().unwrap(),
            &["build-darwin-arm64", "build-darwin-amd64"]
        );
        assert_eq!(p.identifier.as_deref(), Some("com.example.test"));
        assert_eq!(
            p.name.as_deref(),
            Some("{{ ProjectName }}_{{ Version }}_{{ Arch }}")
        );
        assert_eq!(p.install_location.as_deref(), Some("/opt/test/bin"));
        assert_eq!(p.scripts.as_deref(), Some("./scripts/pkg"));
        let extras = p.extra_files.as_ref().unwrap();
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0].glob(), "README.md");
        assert_eq!(extras[1].glob(), "LICENSE");
        assert_eq!(p.replace, Some(true));
        assert_eq!(p.mod_timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(p.skip, Some(StringOrBool::Bool(false)));
    }

    #[test]
    fn test_default_install_location() {
        // When install_location is not set, the stage should default to /usr/local/bin
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            install_location: None,
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        // The default install location is used internally in the pkgbuild command;
        // verify the stage succeeds and registers an artifact (the default is
        // /usr/local/bin which is tested via the pkgbuild_command unit tests).
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1);

        // Verify the default through pkgbuild_command directly
        let cmd = pkgbuild_command(
            "/tmp/staging",
            "com.example.myapp",
            "1.0.0",
            "/usr/local/bin", // the default
            None,
            None,
            "/tmp/out.pkg",
        );
        assert!(
            cmd.contains(&"--install-location".to_string()),
            "command should contain --install-location"
        );
        let loc_idx = cmd.iter().position(|a| a == "--install-location").unwrap();
        assert_eq!(cmd[loc_idx + 1], "/usr/local/bin");
    }

    #[test]
    fn test_extra_files_copied_to_staging() {
        // Run in live mode and verify the stage gets past binary + extra file
        // copying. The outcome depends on which build path is available:
        // pkgbuild (macOS), the Linux flat-package toolchain, or neither.
        let tmp = TempDir::new().unwrap();

        // Create a fake binary
        let binary_dir = tmp.path().join("bin");
        fs::create_dir_all(&binary_dir).unwrap();
        let binary_path = binary_dir.join("myapp");
        fs::write(&binary_path, b"fake binary").unwrap();

        // Create an extra file
        let extra_path = tmp.path().join("README.md");
        fs::write(&extra_path, b"# My App").unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            extra_files: Some(vec![ExtraFileSpec::Glob(
                extra_path.to_string_lossy().into_owned(),
            )]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false, // live mode
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add a darwin binary artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: binary_path,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let result = PkgStage.run(&mut ctx);

        let pkgbuild = anodizer_core::util::find_binary("pkgbuild");
        let linux_toolchain = LINUX_PKG_TOOLS
            .iter()
            .all(|t| anodizer_core::util::find_binary(t))
            && anodizer_core::util::find_binary("sh");

        if pkgbuild {
            // pkgbuild may succeed or fail at exec; either is past the copy step.
            if let Err(e) = &result {
                let err = e.to_string();
                assert!(
                    err.contains("pkgbuild") || err.contains("execute"),
                    "unexpected pkgbuild-path error: {err}"
                );
            }
        } else if linux_toolchain {
            // The Linux flat-package path assembles a real .pkg with no Apple
            // tools, so the live run must succeed and emit the artifact.
            result.expect("Linux flat-package build should succeed");
            let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
            assert_eq!(pkgs.len(), 1, "one .pkg artifact expected");
            assert!(pkgs[0].path.exists(), "emitted .pkg must exist on disk");
        } else {
            // Neither path available => actionable bail naming both options.
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("pkgbuild") && err.contains("xar"),
                "expected dual-option error, got: {err}"
            );
        }
    }

    #[test]
    fn test_invalid_name_template_errors() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            // Invalid Tera template — unclosed tag
            name: Some("{{ bad_template".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let result = PkgStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "invalid name template should cause a render error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("template") || err.contains("render"),
            "error should mention template rendering, got: {err}"
        );
    }

    #[test]
    fn test_ids_filtering() {
        let tmp = TempDir::new().unwrap();

        // Configure ids filter to match only one build id
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            ids: Some(vec!["build-darwin-arm64".to_string()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register two darwin binaries with different metadata ids
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp-arm64"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-darwin-arm64".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp-amd64"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-darwin-amd64".to_string())]),
            size: None,
        });

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        // Verify only one PKG artifact is produced (the arm64 one)
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(
            pkgs.len(),
            1,
            "ids filter should produce exactly one PKG, got {}",
            pkgs.len()
        );
        assert_eq!(
            pkgs[0].target.as_deref(),
            Some("aarch64-apple-darwin"),
            "the PKG should be for the arm64 target"
        );
    }

    // -- `use` field tests --

    #[test]
    fn test_use_appbundle_selects_installer_artifacts() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            use_: Some("appbundle".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register an appbundle artifact (Installer with format=appbundle)
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.app"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
            size: None,
        });

        // Also register a darwin binary that should NOT be selected
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        // Should produce one PKG from the appbundle, not from the binary
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1, "should produce one PKG from the appbundle");
    }

    #[test]
    fn test_use_binary_selects_darwin_binaries() {
        let tmp = TempDir::new().unwrap();

        // Explicit `use: binary` should behave same as omitted (default)
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            use_: Some("binary".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register a darwin binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        // Also register an appbundle that should NOT be selected
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.app"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
            size: None,
        });

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        // Should produce one PKG from the binary, not from the appbundle
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1, "should produce one PKG from the binary");
    }

    #[test]
    fn test_use_default_selects_darwin_binaries() {
        let tmp = TempDir::new().unwrap();

        // No `use_` set — should default to "binary" mode
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(
            pkgs.len(),
            1,
            "default use mode should select darwin binaries"
        );
    }

    #[test]
    fn test_invalid_use_value_errors() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            use_: Some("invalid_mode".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add a binary so the stage tries to process the config
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let result = PkgStage.run(&mut ctx);
        assert!(result.is_err(), "invalid use value should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid `use` value"),
            "error should mention invalid use value, got: {err}"
        );
    }

    #[test]
    fn test_use_appbundle_skips_when_no_appbundles() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            use_: Some("appbundle".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Only register a binary — no appbundles
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        // No PKGs should be produced because there are no appbundle artifacts
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(
            pkgs.len(),
            0,
            "should produce no PKGs when use=appbundle but no appbundles exist"
        );
    }

    // -- StringOrBool disable tests --

    #[test]
    fn test_disable_string_or_bool_true_string() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            skip: Some(StringOrBool::String("true".to_string())),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        // skip: "true" should skip the config
        assert!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty());
    }

    #[test]
    fn test_disable_string_or_bool_false_string() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            skip: Some(StringOrBool::String("false".to_string())),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        // skip: "false" should NOT skip the config
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).len(), 1);
    }

    #[test]
    fn test_disable_string_or_bool_template() {
        let tmp = TempDir::new().unwrap();

        // Template that evaluates to "true" when IsSnapshot is set
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            skip: Some(StringOrBool::String(
                "{% if IsSnapshot %}true{% else %}false{% endif %}".to_string(),
            )),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("IsSnapshot", "true");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        // Template should evaluate to "true", so the config is disabled
        assert!(
            ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty(),
            "template disable should skip the config when evaluated to true"
        );
    }

    #[test]
    fn test_config_parse_pkg_with_use_and_string_disable() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
        use: appbundle
        skip: "{{ if IsSnapshot }}true{{ else }}false{{ endif }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pkgs = config.crates[0].pkgs.as_ref().unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].use_.as_deref(), Some("appbundle"));
        assert!(matches!(pkgs[0].skip, Some(StringOrBool::String(_))));
    }

    // --- `pkg.if` template-conditional ---

    fn pkg_if_test_ctx(if_expr: Option<&str>) -> anodizer_core::context::Context {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::{Config, CrateConfig, PkgConfig};
        use anodizer_core::context::{Context, ContextOptions};
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        std::fs::create_dir_all(&config.dist).unwrap();
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            if_condition: if_expr.map(str::to_string),
            ..Default::default()
        };
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Os", "darwin");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx
    }

    #[test]
    fn test_pkg_if_false_skips_config() {
        use anodizer_core::artifact::ArtifactKind;
        let mut ctx = pkg_if_test_ctx(Some("false"));
        PkgStage.run(&mut ctx).unwrap();
        assert_eq!(
            ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).len(),
            0,
            "pkg if=false should skip"
        );
    }

    #[test]
    fn test_pkg_if_render_failure_is_hard_error() {
        let mut ctx = pkg_if_test_ctx(Some("{{ undefined_function 42 }}"));
        let err = PkgStage
            .run(&mut ctx)
            .expect_err("unrenderable `if` should hard-error");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("`if` template render failed"),
            "error should name `if` render failure, got: {msg}"
        );
    }

    #[test]
    fn test_config_parse_pkg_disable_alias() {
        // The docs show `disable: false`; this must parse without error.
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
        disable: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pkgs = config.crates[0].pkgs.as_ref().unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].skip, Some(StringOrBool::Bool(false)));
    }

    #[test]
    fn test_config_parse_pkg_disable_true_alias() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
        disable: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pkgs = config.crates[0].pkgs.as_ref().unwrap();
        assert_eq!(pkgs[0].skip, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_identifier_template_renders() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.{{ ProjectName }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(
            pkgs[0].metadata.get("identifier").map(|s| s.as_str()),
            Some("com.example.myapp"),
            "identifier template should be rendered"
        );
    }

    /// Build a minimal `Context` with `Version`, `Os`, `Arch`, and `Target` set
    /// so per-binary template renders behave the same as they do inside the
    /// stage loop.
    fn render_fields_test_ctx() -> Context {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Os", "darwin");
        ctx.template_vars_mut().set("Arch", "arm64");
        ctx.template_vars_mut()
            .set("Target", "aarch64-apple-darwin");
        ctx
    }

    #[test]
    fn test_install_location_template_renders() {
        let mut ctx = render_fields_test_ctx();
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            install_location: Some("/opt/{{ ProjectName }}/bin".to_string()),
            ..Default::default()
        };

        let rendered = render_pkg_fields(
            &mut ctx,
            &pkg_cfg,
            pkg_cfg.identifier.as_deref().unwrap(),
            "myapp",
            Some("aarch64-apple-darwin"),
        )
        .unwrap();

        assert_eq!(rendered.install_location, "/opt/myapp/bin");

        let cmd = pkgbuild_command(
            "/tmp/staging",
            &rendered.identifier,
            "1.0.0",
            &rendered.install_location,
            rendered.scripts.as_deref(),
            None,
            "/tmp/out.pkg",
        );
        let loc_idx = cmd.iter().position(|a| a == "--install-location").unwrap();
        assert_eq!(cmd[loc_idx + 1], "/opt/myapp/bin");
    }

    #[test]
    fn test_scripts_template_renders() {
        let mut ctx = render_fields_test_ctx();
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            scripts: Some("scripts/{{ Os }}".to_string()),
            ..Default::default()
        };

        let rendered = render_pkg_fields(
            &mut ctx,
            &pkg_cfg,
            pkg_cfg.identifier.as_deref().unwrap(),
            "myapp",
            Some("aarch64-apple-darwin"),
        )
        .unwrap();

        assert_eq!(rendered.scripts.as_deref(), Some("scripts/darwin"));

        let cmd = pkgbuild_command(
            "/tmp/staging",
            &rendered.identifier,
            "1.0.0",
            &rendered.install_location,
            rendered.scripts.as_deref(),
            None,
            "/tmp/out.pkg",
        );
        let scripts_idx = cmd.iter().position(|a| a == "--scripts").unwrap();
        assert_eq!(cmd[scripts_idx + 1], "scripts/darwin");
    }

    #[test]
    fn test_mod_timestamp_template_renders() {
        let mut ctx = render_fields_test_ctx();
        ctx.template_vars_mut()
            .set("CommitTimestamp", "2024-06-15T12:34:56Z");

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            mod_timestamp: Some("{{ CommitTimestamp }}".to_string()),
            ..Default::default()
        };

        let rendered = render_pkg_fields(
            &mut ctx,
            &pkg_cfg,
            pkg_cfg.identifier.as_deref().unwrap(),
            "myapp",
            Some("aarch64-apple-darwin"),
        )
        .unwrap();

        assert_eq!(
            rendered.mod_timestamp.as_deref(),
            Some("2024-06-15T12:34:56Z"),
            "mod_timestamp template should expand to the CommitTimestamp value, \
             not be passed literally to parse_mod_timestamp"
        );
        assert_ne!(
            rendered.mod_timestamp.as_deref(),
            Some("{{ CommitTimestamp }}"),
            "literal template string must not reach apply_mod_timestamp"
        );
    }

    #[test]
    fn test_default_name_template_no_version_appends_pkg_extension() {
        // Default template has no version segment; the `.pkg` extension is
        // auto-appended so the default emits ProjectName_Arch.pkg.
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1);
        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            filename, "myapp_arm64.pkg",
            "default name template should be ProjectName_Arch with the .pkg extension appended"
        );
    }

    /// A user-supplied `name:` that already ends in `.pkg` is used verbatim —
    /// the auto-append must not double the extension (case-insensitive match).
    #[test]
    fn test_user_name_ending_in_pkg_is_not_doubled() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            name: Some("custom_{{ Arch }}.PKG".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1);
        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            filename, "custom_arm64.PKG",
            "a user name already ending in .pkg (any case) must not get a second .pkg"
        );
    }
}
