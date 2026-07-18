use std::fs;
use std::process::Command;

use anyhow::{Context as _, Result};

// ---------------------------------------------------------------------------
// Linux flat-package builder
// ---------------------------------------------------------------------------

/// Recursively apply `mtime` to every file and directory under `root`.
///
/// [`anodizer_core::util::apply_mod_timestamp`] is files-only; the cpio
/// `Payload` embeds the mtime of every entry it archives — including the
/// directories that hold the install location — so deterministic packages
/// require stamping directories as well as files. `filetime::set_file_mtime`
/// uses `utimensat` rather than `open(O_WRONLY)`, so it stamps directories
/// (which `open(O_WRONLY)` rejects with EISDIR) — leaving directory mtimes at
/// wall-clock would leak non-reproducible bytes into the cpio Payload.
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
pub(crate) fn normalize_odc_cpio(data: &[u8]) -> Vec<u8> {
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
pub(crate) fn normalize_xar_toc(path: &std::path::Path, epoch_secs: i64) -> Result<()> {
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
pub(crate) fn rewrite_toc_fields(toc: &[u8], stamp: &str) -> Vec<u8> {
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
pub(crate) fn epoch_to_iso8601(epoch_secs: i64) -> String {
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
pub(crate) fn sha1_digest(data: &[u8]) -> [u8; 20] {
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
