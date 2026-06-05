//! Crash-safe file writes.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

/// Atomically write `contents` to `path`.
///
/// `fs::write` truncates the target and then streams bytes into it, so a crash
/// (or a full disk) partway through leaves a half-written, corrupt file — fatal
/// when the target is a release artifact, manifest, or checksum that downstream
/// tooling trusts. This instead writes to a uniquely-named temp file in the
/// SAME directory as the target (so the final `rename` is a same-filesystem,
/// atomic operation), `fsync`s that file, and renames it over `path`. The
/// rename either fully succeeds or leaves the previous contents intact; readers
/// never observe a partial write. The temp file is removed on any error before
/// the rename, so a failed write does not litter the directory.
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));

    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("atomic_write: path has no file name: {}", path.display()),
        )
    })?;

    // A process-id + nanosecond suffix keeps concurrent writers to distinct
    // targets in the same directory from colliding on the temp name.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), nanos));
    let tmp_path = dir.join(tmp_name);

    let write_result = (|| -> io::Result<()> {
        let mut file = File::create(&tmp_path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    Ok(())
}

/// Atomically write a string to `path`. Convenience wrapper over
/// [`atomic_write`].
pub fn atomic_write_str(path: &Path, contents: &str) -> io::Result<()> {
    atomic_write(path, contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_full_contents_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("release.txt");
        atomic_write(&target, b"full contents here").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "full contents here");

        // No leftover temp file: the directory should contain exactly the
        // target and nothing matching the `.tmp` suffix.
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["release.txt".to_string()]);
        assert!(
            !entries.iter().any(|n| n.ends_with(".tmp")),
            "temp file was left behind: {entries:?}"
        );
    }

    #[test]
    fn overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("manifest.yaml");
        fs::write(&target, "old").unwrap();
        atomic_write_str(&target, "new value").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new value");
    }
}
