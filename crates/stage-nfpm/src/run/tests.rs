use super::*;

/// Build a job whose "nfpm" is a stub script printing `msg` to stdout
/// (where nfpm reports its errors) and exiting 1.
fn failing_job(dir: &tempfile::TempDir, msg: &str) -> NfpmJob {
    let stub = dir.path().join("nfpm-stub.sh");
    std::fs::write(&stub, format!("#!/bin/sh\necho '{msg}'\nexit 1\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    NfpmJob {
        _tmp_dir: tempfile::TempDir::new().unwrap(),
        pkg_path: dir.path().join("out.msix"),
        format: "msix".to_string(),
        cmd_args: vec![stub.to_string_lossy().into_owned()],
        mtime: None,
        mtime_repr: None,
        extra_env: Vec::new(),
        target: None,
        crate_name: "demo".to_string(),
        pkg_metadata: Default::default(),
    }
}

/// The version-floor hint fires only on the unregistered-packager
/// signature an old nfpm emits — not on other msix failures.
#[test]
#[cfg(unix)]
fn msix_version_floor_hint_scoped_to_unregistered_packager() {
    let dir = tempfile::TempDir::new().unwrap();
    let job = failing_job(&dir, "no packager registered for the format msix");
    let err = execute_nfpm_jobs(&[job], 1, anodizer_core::log::Verbosity::Quiet)
        .expect_err("stub exits 1");
    assert!(
        format!("{err:#}").contains("requires nfpm >= 2.46.0"),
        "hint must fire on the unregistered-packager signature: {err:#}"
    );

    let job = failing_job(&dir, "package msix.applications must be provided");
    let err = execute_nfpm_jobs(&[job], 1, anodizer_core::log::Verbosity::Quiet)
        .expect_err("stub exits 1");
    assert!(
        !format!("{err:#}").contains("requires nfpm >= 2.46.0"),
        "hint must NOT fire on a config-validation failure: {err:#}"
    );
}
