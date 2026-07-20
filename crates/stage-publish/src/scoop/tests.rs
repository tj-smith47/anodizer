#![allow(clippy::field_reassign_with_default)]

use super::*;

#[test]
fn test_generate_manifest() {
    let manifest = generate_manifest(
        "cfgd",
        "1.0.0",
        "https://example.com/cfgd-1.0.0-windows-amd64.zip",
        "sha256xyz",
        "Declarative config management",
        "MIT",
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["version"], "1.0.0");
    assert_eq!(json["architecture"]["64bit"]["hash"], "sha256xyz");
    assert_eq!(json["license"], "MIT");
}

#[test]
fn test_generate_manifest_description() {
    let manifest = generate_manifest(
        "my-tool",
        "2.1.0",
        "https://example.com/my-tool-2.1.0-windows-amd64.zip",
        "deadbeef",
        "A helpful tool",
        "Apache-2.0",
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["description"], "A helpful tool");
    assert_eq!(json["version"], "2.1.0");
    assert_eq!(json["license"], "Apache-2.0");
    assert_eq!(
        json["architecture"]["64bit"]["url"],
        "https://example.com/my-tool-2.1.0-windows-amd64.zip"
    );
}

#[test]
fn compound_spdx_license_emitted_verbatim() {
    // Scoop passes the SPDX license through unchanged: a dual
    // `MIT OR Apache-2.0` expression must land in the manifest's `license`
    // field as the exact string, not split or reshaped.
    let manifest = generate_manifest(
        "my-tool",
        "2.1.0",
        "https://example.com/my-tool-2.1.0-windows-amd64.zip",
        "deadbeef",
        "A helpful tool",
        "MIT OR Apache-2.0",
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["license"], "MIT OR Apache-2.0");
}

// -----------------------------------------------------------------------
// Deep integration tests: verify manifest JSON structure
// -----------------------------------------------------------------------

/// Helper to build a single 64bit ArchEntry for test convenience.
fn arch_64(url: &str, hash: &str) -> Vec<ArchEntry> {
    vec![ArchEntry {
        scoop_arch: "64bit".to_string(),
        url: url.to_string(),
        hash: hash.to_string(),
        wrap_in_directory: None,
    }]
}

#[test]
fn test_integration_manifest_complete_json_structure() {
    let opts = ManifestOptions {
        github_slug: Some("tj-smith47/anodizer".to_string()),
        ..Default::default()
    };
    let entries = arch_64(
        "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-windows-amd64.zip",
        "aabbccdd1122334455667788",
    );
    let manifest = generate_manifest_with_opts(
        "anodizer",
        "3.2.1",
        &entries,
        "Release automation for Rust projects",
        "Apache-2.0",
        &opts,
    )
    .unwrap();

    // Parse the manifest as JSON
    let json: serde_json::Value = serde_json::from_str(&manifest)
        .unwrap_or_else(|e| panic!("manifest should be valid JSON: {e}"));

    // Verify top-level fields exist and have correct values
    assert_eq!(json["version"], "3.2.1");
    assert_eq!(json["description"], "Release automation for Rust projects");
    assert_eq!(json["homepage"], "https://github.com/tj-smith47/anodizer");
    assert_eq!(json["license"], "Apache-2.0");

    // Verify architecture.64bit structure
    let arch_64 = &json["architecture"]["64bit"];
    assert!(
        arch_64.is_object(),
        "architecture.64bit should be an object"
    );
    assert_eq!(
        arch_64["url"],
        "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-windows-amd64.zip"
    );
    assert_eq!(arch_64["hash"], "aabbccdd1122334455667788");
    // `bin` is always an array, even for a single binary.
    assert_eq!(
        arch_64["bin"],
        serde_json::json!(["anodizer.exe"]),
        "single-binary `bin` must still be a JSON array"
    );

    // checkver and autoupdate are NOT emitted.
    assert!(
        json.get("checkver").is_none(),
        "should NOT have checkver key"
    );
    assert!(
        json.get("autoupdate").is_none(),
        "should NOT have autoupdate key"
    );
}

#[test]
fn test_integration_manifest_is_valid_pretty_json() {
    let manifest = generate_manifest(
        "my-tool",
        "1.5.0",
        "https://example.com/my-tool-1.5.0-windows-amd64.zip",
        "deadbeefcafebabe",
        "A useful tool",
        "MIT",
    )
    .unwrap();

    // Verify it is pretty-printed (has newlines and indentation)
    assert!(manifest.contains('\n'), "should be pretty-printed");
    assert!(manifest.contains("  "), "should have indentation");

    // Verify it can be re-parsed
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();

    // Verify all expected top-level keys
    let obj = json.as_object().unwrap();
    let keys: Vec<&String> = obj.keys().collect();
    assert!(
        keys.iter().any(|k| k.as_str() == "version"),
        "should have version key"
    );
    assert!(
        keys.iter().any(|k| k.as_str() == "description"),
        "should have description key"
    );
    assert!(
        keys.iter().any(|k| k.as_str() == "homepage"),
        "should have homepage key"
    );
    assert!(
        keys.iter().any(|k| k.as_str() == "license"),
        "should have license key"
    );
    assert!(
        keys.iter().any(|k| k.as_str() == "architecture"),
        "should have architecture key"
    );
    // checkver and autoupdate are only present when github_slug is set
    assert!(
        !keys.iter().any(|k| k.as_str() == "checkver"),
        "should NOT have checkver key when github_slug is absent"
    );
    assert!(
        !keys.iter().any(|k| k.as_str() == "autoupdate"),
        "should NOT have autoupdate key when github_slug is absent"
    );
}

#[test]
fn test_integration_manifest_special_characters_in_description() {
    let manifest = generate_manifest(
        "json-tool",
        "1.0.0",
        "https://example.com/tool.zip",
        "hash123",
        "A tool for \"parsing\" JSON & XML <data>",
        "MIT",
    )
    .unwrap();

    // Even with special characters, should produce valid JSON
    let json: serde_json::Value = serde_json::from_str(&manifest)
        .unwrap_or_else(|e| panic!("manifest with special chars should still be valid JSON: {e}"));
    assert_eq!(
        json["description"],
        "A tool for \"parsing\" JSON & XML <data>"
    );
}

#[test]
fn test_integration_manifest_bin_matches_name() {
    // Verify that the bin field in the manifest matches the name parameter
    let manifest = generate_manifest(
        "my-special-cli",
        "0.1.0",
        "https://example.com/cli.zip",
        "abc",
        "desc",
        "MIT",
    )
    .unwrap();

    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(
        json["architecture"]["64bit"]["bin"],
        serde_json::json!(["my-special-cli.exe"]),
        "bin should match the tool name (always an array)"
    );
}

#[test]
fn test_manifest_no_autoupdate_even_with_slug() {
    // checkver/autoupdate are never emitted.
    let opts = ManifestOptions {
        github_slug: Some("myorg/release-tool".to_string()),
        ..Default::default()
    };
    let entries = arch_64(
        "https://example.com/release-tool-5.0.0-windows-amd64.zip",
        "hash",
    );
    let manifest =
        generate_manifest_with_opts("release-tool", "5.0.0", &entries, "desc", "MIT", &opts)
            .unwrap();

    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert!(
        json.get("checkver").is_none(),
        "should NOT have checkver key"
    );
    assert!(
        json.get("autoupdate").is_none(),
        "should NOT have autoupdate key"
    );
}

// -----------------------------------------------------------------------
// Additional behavior tests — config fields actually do things
// -----------------------------------------------------------------------

#[test]
fn test_scoop_manifest_architecture_structure() {
    let manifest = generate_manifest(
        "myapp",
        "1.0.0",
        "https://example.com/myapp-1.0.0-windows-amd64.zip",
        "deadbeef",
        "My application",
        "Apache-2.0",
    )
    .unwrap();

    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();

    // Verify architecture.64bit has all expected fields
    let arch64 = &json["architecture"]["64bit"];
    assert_eq!(
        arch64["url"],
        "https://example.com/myapp-1.0.0-windows-amd64.zip"
    );
    assert_eq!(arch64["hash"], "deadbeef");
    assert_eq!(
        arch64["bin"],
        serde_json::json!(["myapp.exe"]),
        "single-binary `bin` must still be a JSON array"
    );
}

#[test]
fn test_scoop_manifest_no_checkver_autoupdate_with_slug() {
    // checkver/autoupdate are never emitted, even with a slug.
    let opts = ManifestOptions {
        github_slug: Some("myorg/mytool".to_string()),
        ..Default::default()
    };
    let entries = arch_64("https://example.com/mytool.zip", "abc");
    let manifest =
        generate_manifest_with_opts("mytool", "2.0.0", &entries, "desc", "MIT", &opts).unwrap();

    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert!(
        json.get("checkver").is_none(),
        "should NOT have checkver key"
    );
    assert!(
        json.get("autoupdate").is_none(),
        "should NOT have autoupdate key"
    );
}

#[test]
fn test_scoop_manifest_no_checkver_autoupdate_without_slug() {
    let manifest = generate_manifest(
        "mytool",
        "2.0.0",
        "https://example.com/mytool.zip",
        "abc",
        "desc",
        "MIT",
    )
    .unwrap();

    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert!(
        json.get("checkver").is_none(),
        "checkver should be absent without github_slug"
    );
    assert!(
        json.get("autoupdate").is_none(),
        "autoupdate should be absent without github_slug"
    );
}

#[test]
fn test_scoop_manifest_homepage_derived_from_name() {
    let manifest = generate_manifest(
        "my-tool",
        "1.0.0",
        "https://example.com/t.zip",
        "hash",
        "desc",
        "MIT",
    )
    .unwrap();

    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["homepage"], "https://github.com/my-tool");
}

// -----------------------------------------------------------------------
// New fields: homepage, persist, depends, pre/post_install, shortcuts
// -----------------------------------------------------------------------

#[test]
fn test_manifest_custom_homepage() {
    let opts = ManifestOptions {
        homepage: Some("https://example.com/mytool"),
        ..Default::default()
    };
    let entries = arch_64("https://example.com/a.zip", "abc");
    let manifest =
        generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["homepage"], "https://example.com/mytool");
}

#[test]
fn test_manifest_homepage_fallback() {
    let manifest = generate_manifest(
        "mytool",
        "1.0.0",
        "https://example.com/a.zip",
        "abc",
        "desc",
        "MIT",
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["homepage"], "https://github.com/mytool");
}

#[test]
fn test_manifest_persist() {
    let persist = vec!["data".to_string(), "config.ini".to_string()];
    let opts = ManifestOptions {
        persist: Some(&persist),
        ..Default::default()
    };
    let entries = arch_64("https://example.com/a.zip", "abc");
    let manifest =
        generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    let arr = json["persist"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0], "data");
    assert_eq!(arr[1], "config.ini");
}

#[test]
fn test_manifest_depends() {
    let depends = vec!["git".to_string(), "7zip".to_string()];
    let opts = ManifestOptions {
        depends: Some(&depends),
        ..Default::default()
    };
    let entries = arch_64("https://example.com/a.zip", "abc");
    let manifest =
        generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    let arr = json["depends"].as_array().unwrap();
    assert_eq!(arr, &["git", "7zip"]);
}

#[test]
fn test_manifest_pre_install() {
    let pre = vec!["Write-Host 'Installing...'".to_string()];
    let opts = ManifestOptions {
        pre_install: Some(&pre),
        ..Default::default()
    };
    let entries = arch_64("https://example.com/a.zip", "abc");
    let manifest =
        generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    let arr = json["pre_install"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0], "Write-Host 'Installing...'");
}

#[test]
fn test_manifest_post_install() {
    let post = vec!["Write-Host 'Done!'".to_string()];
    let opts = ManifestOptions {
        post_install: Some(&post),
        ..Default::default()
    };
    let entries = arch_64("https://example.com/a.zip", "abc");
    let manifest =
        generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    let arr = json["post_install"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0], "Write-Host 'Done!'");
}

#[test]
fn test_manifest_shortcuts() {
    let shortcuts = vec![
        vec!["myapp.exe".to_string(), "My App".to_string()],
        vec![
            "myapp.exe".to_string(),
            "My App CLI".to_string(),
            "--cli".to_string(),
        ],
    ];
    let opts = ManifestOptions {
        shortcuts: Some(&shortcuts),
        ..Default::default()
    };
    let entries = arch_64("https://example.com/a.zip", "abc");
    let manifest =
        generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    let arr = json["shortcuts"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0][0], "myapp.exe");
    assert_eq!(arr[0][1], "My App");
    assert_eq!(arr[1][2], "--cli");
}

#[test]
fn test_manifest_no_optional_fields_when_not_set() {
    let manifest = generate_manifest(
        "mytool",
        "1.0.0",
        "https://example.com/a.zip",
        "abc",
        "desc",
        "MIT",
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert!(json.get("persist").is_none());
    assert!(json.get("depends").is_none());
    assert!(json.get("pre_install").is_none());
    assert!(json.get("post_install").is_none());
    assert!(json.get("shortcuts").is_none());
}

#[test]
fn test_manifest_all_new_fields_together() {
    let persist = vec!["data".to_string()];
    let depends = vec!["git".to_string()];
    let pre = vec!["echo pre".to_string()];
    let post = vec!["echo post".to_string()];
    let shortcuts = vec![vec!["app.exe".to_string(), "App".to_string()]];
    let opts = ManifestOptions {
        homepage: Some("https://example.com"),
        github_slug: None,
        persist: Some(&persist),
        depends: Some(&depends),
        pre_install: Some(&pre),
        post_install: Some(&post),
        shortcuts: Some(&shortcuts),
        bin: None,
        checkver: None,
        autoupdate_hash: None,
    };
    let entries = arch_64("https://example.com/a.zip", "abc");
    let manifest =
        generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["homepage"], "https://example.com");
    assert!(json["persist"].is_array());
    assert!(json["depends"].is_array());
    assert!(json["pre_install"].is_array());
    assert!(json["post_install"].is_array());
    assert!(json["shortcuts"].is_array());
}

// -----------------------------------------------------------------------
// Multi-arch manifest tests (32bit + 64bit + arm64)
// -----------------------------------------------------------------------

#[test]
fn test_manifest_multi_arch_all_three() {
    let entries = vec![
        ArchEntry {
            scoop_arch: "64bit".to_string(),
            url: "https://example.com/app-1.0.0-windows-amd64.zip".to_string(),
            hash: "hash_amd64".to_string(),
            wrap_in_directory: None,
        },
        ArchEntry {
            scoop_arch: "32bit".to_string(),
            url: "https://example.com/app-1.0.0-windows-386.zip".to_string(),
            hash: "hash_386".to_string(),
            wrap_in_directory: None,
        },
        ArchEntry {
            scoop_arch: "arm64".to_string(),
            url: "https://example.com/app-1.0.0-windows-arm64.zip".to_string(),
            hash: "hash_arm64".to_string(),
            wrap_in_directory: None,
        },
    ];
    let opts = ManifestOptions {
        github_slug: Some("myorg/app".to_string()),
        ..Default::default()
    };
    let manifest =
        generate_manifest_with_opts("app", "1.0.0", &entries, "A multi-arch app", "MIT", &opts)
            .unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();

    // Verify all three architecture blocks
    let arch = &json["architecture"];
    assert!(arch["64bit"].is_object(), "64bit block should exist");
    assert!(arch["32bit"].is_object(), "32bit block should exist");
    assert!(arch["arm64"].is_object(), "arm64 block should exist");

    // Verify URLs and hashes
    assert_eq!(
        arch["64bit"]["url"],
        "https://example.com/app-1.0.0-windows-amd64.zip"
    );
    assert_eq!(arch["64bit"]["hash"], "hash_amd64");
    assert_eq!(arch["64bit"]["bin"], serde_json::json!(["app.exe"]));

    assert_eq!(
        arch["32bit"]["url"],
        "https://example.com/app-1.0.0-windows-386.zip"
    );
    assert_eq!(arch["32bit"]["hash"], "hash_386");
    assert_eq!(arch["32bit"]["bin"], serde_json::json!(["app.exe"]));

    assert_eq!(
        arch["arm64"]["url"],
        "https://example.com/app-1.0.0-windows-arm64.zip"
    );
    assert_eq!(arch["arm64"]["hash"], "hash_arm64");
    assert_eq!(arch["arm64"]["bin"], serde_json::json!(["app.exe"]));

    // checkver/autoupdate are never emitted.
    assert!(
        json.get("checkver").is_none(),
        "should NOT have checkver key"
    );
    assert!(
        json.get("autoupdate").is_none(),
        "should NOT have autoupdate key"
    );
}

// -----------------------------------------------------------------------
// wrap_in_directory tests
// -----------------------------------------------------------------------

#[test]
fn test_manifest_wrap_in_directory_single_bin() {
    let entries = vec![ArchEntry {
        scoop_arch: "64bit".to_string(),
        url: "https://example.com/app-1.0.0-windows-amd64.zip".to_string(),
        hash: "hash123".to_string(),
        wrap_in_directory: Some("app-1.0.0".to_string()),
    }];
    let manifest = generate_manifest_with_opts(
        "app",
        "1.0.0",
        &entries,
        "An app",
        "MIT",
        &ManifestOptions::default(),
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    // A wrapping archive keeps `bin` as a flat array of plain exe
    // names and expresses the wrap dir once via per-arch `extract_dir`
    // (matching real ripgrep/fd). The old baked `["dir/bin.exe", alias]`
    // pair broke `scoop which` / shortcut resolution.
    let arch = &json["architecture"]["64bit"];
    assert_eq!(
        arch["bin"],
        serde_json::json!(["app.exe"]),
        "bin must be a flat array of plain exe names, got:\n{arch}"
    );
    assert_eq!(
        arch["extract_dir"], "app-1.0.0",
        "extract_dir must carry the wrap directory, got:\n{arch}"
    );
}

#[test]
fn test_manifest_wrap_in_directory_multiple_bins() {
    let entries = vec![ArchEntry {
        scoop_arch: "64bit".to_string(),
        url: "https://example.com/suite-1.0.0.zip".to_string(),
        hash: "hash456".to_string(),
        wrap_in_directory: Some("suite-1.0.0".to_string()),
    }];
    let bins = vec!["cli".to_string(), "daemon".to_string()];
    let opts = ManifestOptions {
        bin: Some(&bins),
        ..Default::default()
    };
    let manifest =
        generate_manifest_with_opts("suite", "1.0.0", &entries, "A suite", "MIT", &opts).unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    // Multiple binaries stay flat; extract_dir is shared once.
    let arch = &json["architecture"]["64bit"];
    assert_eq!(
        arch["bin"],
        serde_json::json!(["cli.exe", "daemon.exe"]),
        "bin must be a flat array of plain exe names, got:\n{arch}"
    );
    assert_eq!(arch["extract_dir"], "suite-1.0.0");
}

#[test]
fn test_manifest_no_wrap_emits_bin_as_array() {
    let entries = vec![ArchEntry {
        scoop_arch: "64bit".to_string(),
        url: "https://example.com/app.zip".to_string(),
        hash: "hash789".to_string(),
        wrap_in_directory: None,
    }];
    let manifest = generate_manifest_with_opts(
        "app",
        "1.0.0",
        &entries,
        "An app",
        "MIT",
        &ManifestOptions::default(),
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    // Without wrap_in_directory, single-binary `bin` is still a
    // JSON array, not a bare string.
    let arch = &json["architecture"]["64bit"];
    assert_eq!(
        arch["bin"],
        serde_json::json!(["app.exe"]),
        "single-binary `bin` must still be a JSON array"
    );
    // A FLAT archive must NOT carry extract_dir (scoop would look for
    // a non-existent subdir).
    assert!(
        arch.get("extract_dir").is_none(),
        "flat archive must not emit extract_dir, got:\n{arch}"
    );
}

// -----------------------------------------------------------------------
// checkver + autoupdate
// -----------------------------------------------------------------------

/// With `checkver` + sidecar hash mode, the manifest carries
/// `checkver: github` and an `autoupdate` block whose per-arch url has the
/// version templated to `$version` and whose hash points at `$url.sha256`
/// — the exact shape real ripgrep/fd scoop manifests use for sidecars.
#[test]
fn test_scoop_checkver_and_autoupdate_sidecar() {
    let entries = vec![ArchEntry {
        scoop_arch: "64bit".to_string(),
        url: "https://github.com/owner/repo/releases/download/v1.2.3/repo-1.2.3-x86_64-pc-windows-msvc.zip".to_string(),
        hash: "abc123".to_string(),
        wrap_in_directory: Some("repo-1.2.3-x86_64-pc-windows-msvc".to_string()),
    }];
    let opts = ManifestOptions {
        github_slug: Some("owner/repo".to_string()),
        checkver: Some("github".to_string()),
        autoupdate_hash: Some(AutoupdateHash::UrlSidecar {
            suffix: "sha256".to_string(),
        }),
        ..Default::default()
    };
    let manifest =
        generate_manifest_with_opts("repo", "1.2.3", &entries, "desc", "MIT", &opts).unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();

    assert_eq!(json["checkver"], "github");
    let au = &json["autoupdate"];
    assert_eq!(
        au["architecture"]["64bit"]["url"],
        "https://github.com/owner/repo/releases/download/v$version/repo-$version-x86_64-pc-windows-msvc.zip",
        "autoupdate url must template the version with $version, got:\n{au}"
    );
    assert_eq!(
        au["architecture"]["64bit"]["extract_dir"], "repo-$version-x86_64-pc-windows-msvc",
        "autoupdate extract_dir must template the version, got:\n{au}"
    );
    assert_eq!(
        au["hash"]["url"], "$url.sha256",
        "sidecar mode → hash.url = $url.sha256"
    );
}

/// Combined-checksums mode points the autoupdate hash at the
/// version-templated checksums file URL plus a per-asset extraction regex.
#[test]
fn test_scoop_autoupdate_combined_checksums() {
    let entries = vec![ArchEntry {
        scoop_arch: "64bit".to_string(),
        url: "https://github.com/owner/repo/releases/download/v2.0.0/repo-2.0.0-windows-amd64.zip"
            .to_string(),
        hash: "abc".to_string(),
        wrap_in_directory: None,
    }];
    let opts = ManifestOptions {
        github_slug: Some("owner/repo".to_string()),
        checkver: Some("github".to_string()),
        autoupdate_hash: Some(AutoupdateHash::ChecksumsRegex {
            url_template: "https://github.com/owner/repo/releases/download/v$version/repo_$version_checksums.txt".to_string(),
        }),
        ..Default::default()
    };
    let manifest =
        generate_manifest_with_opts("repo", "2.0.0", &entries, "desc", "MIT", &opts).unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(
        json["autoupdate"]["hash"]["url"],
        "https://github.com/owner/repo/releases/download/v$version/repo_$version_checksums.txt"
    );
    assert_eq!(json["autoupdate"]["hash"]["regex"], "$sha256\\s+$basename");
}

/// With no autoupdate hash mode resolvable, NEITHER checkver nor
/// autoupdate is emitted (a checkver without autoupdate is a dead
/// half-manifest).
#[test]
fn test_scoop_no_autoupdate_omits_both_keys() {
    let entries = vec![ArchEntry {
        scoop_arch: "64bit".to_string(),
        url: "https://example.com/app.zip".to_string(),
        hash: "h".to_string(),
        wrap_in_directory: None,
    }];
    let opts = ManifestOptions {
        checkver: Some("github".to_string()),
        autoupdate_hash: None,
        ..Default::default()
    };
    let manifest =
        generate_manifest_with_opts("app", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert!(json.get("checkver").is_none());
    assert!(json.get("autoupdate").is_none());
}

// -----------------------------------------------------------------------
// skip_upload tests (reuses should_skip_upload from homebrew)
// -----------------------------------------------------------------------

// -----------------------------------------------------------------------
// Scoop manifest name override
// -----------------------------------------------------------------------

#[test]
fn test_manifest_name_override() {
    // When ScoopConfig.name is set, the manifest bin and filename should
    // use the override name.
    let manifest = generate_manifest(
        "custom-name",
        "1.0.0",
        "https://example.com/custom-name-1.0.0-windows-amd64.zip",
        "abc123",
        "A custom named tool",
        "MIT",
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(
        json["architecture"]["64bit"]["bin"],
        serde_json::json!(["custom-name.exe"])
    );
}

// -----------------------------------------------------------------------
// Scoop manifest directory placement (dry-run test)
// -----------------------------------------------------------------------

// -----------------------------------------------------------------------
// Scoop commit message template (uses shared render_commit_msg)
// -----------------------------------------------------------------------

#[test]
fn test_scoop_commit_msg_default() {
    // Canonical default: "Scoop update for {{ .ProjectName }} version {{ .Tag }}"
    let scoop_default = "Scoop update for {{ ProjectName }} version {{ Tag }}";
    let log =
        anodizer_core::log::StageLogger::new("publish", anodizer_core::log::Verbosity::Normal);
    let msg = crate::homebrew::render_commit_msg(
        Some(scoop_default),
        "mytool",
        "1.2.3",
        "manifest",
        &log,
        false,
    )
    .unwrap();
    assert_eq!(msg, "Scoop update for mytool version 1.2.3");
}

#[test]
fn test_scoop_commit_msg_custom() {
    let log =
        anodizer_core::log::StageLogger::new("publish", anodizer_core::log::Verbosity::Normal);
    let msg = crate::homebrew::render_commit_msg(
        Some("scoop: bump {{ name }} to {{ version }}"),
        "mytool",
        "3.0.0",
        "manifest",
        &log,
        false,
    )
    .unwrap();
    assert_eq!(msg, "scoop: bump mytool to 3.0.0");
}

// -----------------------------------------------------------------------
// Multi-artifact disambiguation tests
// -----------------------------------------------------------------------

use anodizer_core::log::{StageLogger, Verbosity};

fn arch_entry(scoop_arch: &str, url: &str, hash: &str) -> ArchEntry {
    ArchEntry {
        scoop_arch: scoop_arch.to_string(),
        url: url.to_string(),
        hash: hash.to_string(),
        wrap_in_directory: None,
    }
}

fn test_log() -> StageLogger {
    StageLogger::new("publish", Verbosity::Normal)
}

/// Extract the error message from a `Result<Vec<ArchEntry>>`. `.unwrap_err()`
/// is unusable here because `ArchEntry` deliberately doesn't derive `Debug`.
fn expect_err(result: anyhow::Result<Vec<ArchEntry>>) -> String {
    match result {
        Ok(_) => panic!("expected error, got Ok"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn test_disambiguate_arch_entries_single_per_arch_unchanged() {
    let entries = vec![
        (
            arch_entry("64bit", "https://example.com/tool-amd64.zip", "sha64"),
            "zip".to_string(),
        ),
        (
            arch_entry("arm64", "https://example.com/tool-arm64.zip", "shaarm"),
            "zip".to_string(),
        ),
    ];
    let result = disambiguate_arch_entries(entries, false, "anodizer", &test_log()).unwrap();
    assert_eq!(result.len(), 2);
    let amd = result
        .iter()
        .find(|e| e.scoop_arch == "64bit")
        .expect("64bit missing");
    assert_eq!(amd.url, "https://example.com/tool-amd64.zip");
    assert_eq!(amd.hash, "sha64");
    let arm = result
        .iter()
        .find(|e| e.scoop_arch == "arm64")
        .expect("arm64 missing");
    assert_eq!(arm.url, "https://example.com/tool-arm64.zip");
    assert_eq!(arm.hash, "shaarm");
}

#[test]
fn test_disambiguate_arch_entries_deterministic_order() {
    // Same input must produce the same output order across runs.
    let entries = || {
        vec![
            (
                arch_entry("arm64", "https://example.com/tool-arm64.zip", "shaarm"),
                "zip".to_string(),
            ),
            (
                arch_entry("64bit", "https://example.com/tool-amd64.zip", "sha64"),
                "zip".to_string(),
            ),
            (
                arch_entry("32bit", "https://example.com/tool-i386.zip", "sha32"),
                "zip".to_string(),
            ),
        ]
    };
    let r1 = disambiguate_arch_entries(entries(), false, "anodizer", &test_log()).unwrap();
    let r2 = disambiguate_arch_entries(entries(), false, "anodizer", &test_log()).unwrap();
    let keys1: Vec<&str> = r1.iter().map(|e| e.scoop_arch.as_str()).collect();
    let keys2: Vec<&str> = r2.iter().map(|e| e.scoop_arch.as_str()).collect();
    assert_eq!(keys1, keys2, "disambiguation order must be deterministic");
}

#[test]
fn test_disambiguate_arch_entries_prefers_zip_over_tar_gz() {
    // 64bit appears with both .zip and .tar.gz; zip must win.
    let entries = vec![
        (
            arch_entry("64bit", "https://example.com/tool-amd64.tar.gz", "sha_tgz"),
            "tar.gz".to_string(),
        ),
        (
            arch_entry("64bit", "https://example.com/tool-amd64.zip", "sha_zip"),
            "zip".to_string(),
        ),
    ];
    let result = disambiguate_arch_entries(entries, false, "anodizer", &test_log()).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].hash, "sha_zip", "expected zip to be selected");
}

#[test]
fn test_disambiguate_arch_entries_prefers_tar_gz_when_no_zip() {
    // 64bit with tar.gz and tar.xz; tar.gz must win.
    let entries = vec![
        (
            arch_entry("64bit", "https://example.com/tool-amd64.tar.xz", "sha_xz"),
            "tar.xz".to_string(),
        ),
        (
            arch_entry("64bit", "https://example.com/tool-amd64.tar.gz", "sha_gz"),
            "tar.gz".to_string(),
        ),
    ];
    let result = disambiguate_arch_entries(entries, false, "anodizer", &test_log()).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].hash, "sha_gz", "expected tar.gz to be selected");
}

#[test]
fn test_disambiguate_arch_entries_errors_when_ids_set_and_duplicate() {
    let entries = vec![
        (
            arch_entry("64bit", "https://example.com/tool-a.zip", "sha_a"),
            "zip".to_string(),
        ),
        (
            arch_entry("64bit", "https://example.com/tool-b.zip", "sha_b"),
            "zip".to_string(),
        ),
    ];
    let msg = expect_err(disambiguate_arch_entries(
        entries,
        true,
        "anodizer",
        &test_log(),
    ));
    assert!(msg.starts_with("scoop:"), "missing prefix: {msg}");
    assert!(
        msg.contains("crate 'anodizer'"),
        "missing crate name: {msg}"
    );
    assert!(
        msg.contains("multiple archives found for"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("tool-a.zip") && msg.contains("tool-b.zip"),
        "error must name conflicting artifacts: {msg}"
    );
}

#[test]
fn test_disambiguate_arch_entries_errors_when_no_preferred_format() {
    // Two non-preferred formats for the same arch, ids unset → error.
    let entries = vec![
        (
            arch_entry("64bit", "https://example.com/tool.tar.xz", "sha_xz"),
            "tar.xz".to_string(),
        ),
        (
            arch_entry("64bit", "https://example.com/tool.tar.zst", "sha_zst"),
            "tar.zst".to_string(),
        ),
    ];
    let msg = expect_err(disambiguate_arch_entries(
        entries,
        false,
        "anodizer",
        &test_log(),
    ));
    assert!(msg.starts_with("scoop:"), "missing prefix: {msg}");
    assert!(
        msg.contains("crate 'anodizer'"),
        "missing crate name: {msg}"
    );
    assert!(
        msg.contains("none matches a preferred format"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("tool.tar.xz") && msg.contains("tool.tar.zst"),
        "error must name conflicting artifacts: {msg}"
    );
}

#[test]
fn test_disambiguate_arch_entries_errors_when_multiple_tar_gz_no_zip() {
    // Two tar.gz archives for the same arch with no zip and ids unset.
    // Previous code path misreported this as "multiple .zip artifacts";
    // the correct error names tar.gz as the conflicting bucket.
    let entries = vec![
        (
            arch_entry("64bit", "https://example.com/tool-A.tar.gz", "sha_a"),
            "tar.gz".to_string(),
        ),
        (
            arch_entry("64bit", "https://example.com/tool-B.tar.gz", "sha_b"),
            "tar.gz".to_string(),
        ),
    ];
    let msg = expect_err(disambiguate_arch_entries(
        entries,
        false,
        "anodizer",
        &test_log(),
    ));
    assert!(msg.starts_with("scoop:"), "missing prefix: {msg}");
    assert!(
        msg.contains("multiple .tar.gz archives"),
        "expected tar.gz to be named in error, got: {msg}"
    );
    assert!(
        !msg.contains("multiple .zip"),
        "must not blame zip when there is none: {msg}"
    );
    assert!(
        msg.contains("tool-A.tar.gz") && msg.contains("tool-B.tar.gz"),
        "error must name conflicting artifacts: {msg}"
    );
}

#[test]
fn test_disambiguate_arch_entries_ids_set_no_duplicates_passes() {
    // ids_was_set=true with one entry per arch — pass-through OK.
    let entries = vec![
        (
            arch_entry("64bit", "https://example.com/tool-amd64.zip", "sha64"),
            "zip".to_string(),
        ),
        (
            arch_entry("arm64", "https://example.com/tool-arm64.zip", "shaarm"),
            "zip".to_string(),
        ),
    ];
    let result = disambiguate_arch_entries(entries, true, "anodizer", &test_log()).unwrap();
    assert_eq!(result.len(), 2);
}

#[test]
fn test_disambiguate_arch_entries_empty_input() {
    let result = disambiguate_arch_entries(vec![], false, "anodizer", &test_log()).unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_disambiguate_arch_entries_logs_dropped_via_sink() {
    // Two archives for the same scoop_arch with ids unset: the fallback
    // keeps the .zip and drops the .tar.gz. Capture the warn sink to
    // assert both URLs appear in the emitted log line.
    let entries = vec![
        (
            arch_entry("64bit", "https://example.com/tool-amd64.tar.gz", "sha_tgz"),
            "tar.gz".to_string(),
        ),
        (
            arch_entry("64bit", "https://example.com/tool-amd64.zip", "sha_zip"),
            "zip".to_string(),
        ),
    ];
    let mut captured: Vec<String> = Vec::new();
    let result = crate::util::disambiguate_by_format_with_sink(
        entries,
        |(entry, _)| entry.scoop_arch.clone(),
        |(_, fmt)| fmt.as_str(),
        |(entry, _)| entry.url.clone(),
        crate::util::DisambiguateInnerConfig {
            preferred_formats: super::SCOOP_PREFERRED_FORMATS,
            ids_was_set: false,
            publisher_label: "scoop",
            crate_name: "anodizer",
        },
        &mut |msg| captured.push(msg.to_string()),
    )
    .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(captured.len(), 1, "expected exactly one warn line");
    let line = &captured[0];
    assert!(
        line.starts_with("scoop:"),
        "warn line should carry publisher prefix: {line}"
    );
    assert!(
        line.contains("crate 'anodizer'"),
        "warn line should name the crate: {line}"
    );
    assert!(
        line.contains("tool-amd64.zip") && line.contains("(.zip)"),
        "warn line should name the kept archive: {line}"
    );
    assert!(
        line.contains("tool-amd64.tar.gz") && line.contains("(.tar.gz)"),
        "warn line should name the dropped archive: {line}"
    );
}
