//! GitHub client trait and mock implementation.
//!
//! Defines the [`GitHubClient`] trait that abstracts GitHub API operations
//! needed by the release stage. The real octocrab-based implementation lives
//! in `crates/stage-release`; this module provides only the trait definition
//! and a [`MockGitHubClient`] for testing.
//!
//! # Usage
//!
//! The mock client records every call and returns configurable responses:
//!
//! ```rust,ignore
//! use anodize_core::github_client::{MockGitHubClient, ReleaseInfo, GitHubClient};
//!
//! let mock = MockGitHubClient::new();
//! mock.set_create_release_response(Ok(ReleaseInfo {
//!     id: 42,
//!     html_url: "https://github.com/owner/repo/releases/42".to_string(),
//!     tag_name: "v1.0.0".to_string(),
//!     name: Some("Release v1.0.0".to_string()),
//!     draft: false,
//! }));
//!
//! let result = mock.create_release(&params).unwrap();
//! assert_eq!(mock.create_release_calls(), 1);
//! ```

use std::path::PathBuf;

#[cfg(feature = "test-helpers")]
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Minimal release metadata returned by GitHub API operations.
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub id: u64,
    pub html_url: String,
    pub tag_name: String,
    pub name: Option<String>,
    pub draft: bool,
}

/// Parameters for creating a GitHub release.
#[derive(Debug, Clone)]
pub struct CreateReleaseParams {
    pub owner: String,
    pub repo: String,
    pub tag_name: String,
    pub name: String,
    pub body: String,
    pub draft: bool,
    pub prerelease: bool,
    pub generate_release_notes: bool,
    pub make_latest: Option<String>,
}

/// Parameters for uploading a release asset.
#[derive(Debug, Clone)]
pub struct UploadAssetParams {
    pub owner: String,
    pub repo: String,
    pub release_id: u64,
    pub file_name: String,
    pub file_path: PathBuf,
}

/// Minimal asset metadata returned by upload operations.
#[derive(Debug, Clone)]
pub struct AssetInfo {
    pub id: u64,
    pub name: String,
    pub size: u64,
}

/// Parameters for listing releases.
#[derive(Debug, Clone)]
pub struct ListReleasesParams {
    pub owner: String,
    pub repo: String,
}

/// Parameters for deleting a release.
#[derive(Debug, Clone)]
pub struct DeleteReleaseParams {
    pub owner: String,
    pub repo: String,
    pub release_id: u64,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Abstraction over GitHub API operations used by the release stage.
///
/// Implementations:
/// - Real: wraps octocrab (lives in `crates/stage-release`)
/// - Mock: [`MockGitHubClient`] for tests (records calls, configurable responses)
pub trait GitHubClient {
    /// Create a new GitHub release.
    fn create_release(&self, params: &CreateReleaseParams) -> anyhow::Result<ReleaseInfo>;

    /// Upload an asset to an existing release.
    fn upload_asset(&self, params: &UploadAssetParams) -> anyhow::Result<AssetInfo>;

    /// List all releases for a repository.
    fn list_releases(&self, params: &ListReleasesParams) -> anyhow::Result<Vec<ReleaseInfo>>;

    /// Delete a release by ID.
    fn delete_release(&self, params: &DeleteReleaseParams) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// MockGitHubClient (test-only)
// ---------------------------------------------------------------------------

/// A mock GitHub client that records calls and returns configurable responses.
///
/// All response setters use interior mutability (Mutex) so the mock can be
/// shared across test code without requiring `&mut`.
///
/// Only available when the `test-helpers` feature is enabled.
#[cfg(feature = "test-helpers")]
pub struct MockGitHubClient {
    create_release_calls: Mutex<Vec<CreateReleaseParams>>,
    upload_asset_calls: Mutex<Vec<UploadAssetParams>>,
    list_releases_calls: Mutex<Vec<ListReleasesParams>>,
    delete_release_calls: Mutex<Vec<DeleteReleaseParams>>,

    create_release_response: Mutex<Option<Result<ReleaseInfo, String>>>,
    upload_asset_response: Mutex<Option<Result<AssetInfo, String>>>,
    list_releases_response: Mutex<Option<Result<Vec<ReleaseInfo>, String>>>,
    delete_release_response: Mutex<Option<Result<(), String>>>,
}

#[cfg(feature = "test-helpers")]
impl MockGitHubClient {
    /// Create a new mock with no pre-configured responses.
    ///
    /// By default, all operations will return an error saying
    /// "no mock response configured". Use the `set_*_response` methods
    /// to configure what each operation returns.
    pub fn new() -> Self {
        Self {
            create_release_calls: Mutex::new(Vec::new()),
            upload_asset_calls: Mutex::new(Vec::new()),
            list_releases_calls: Mutex::new(Vec::new()),
            delete_release_calls: Mutex::new(Vec::new()),
            create_release_response: Mutex::new(None),
            upload_asset_response: Mutex::new(None),
            list_releases_response: Mutex::new(None),
            delete_release_response: Mutex::new(None),
        }
    }

    // -- Response setters --

    /// Configure the response for `create_release` calls.
    pub fn set_create_release_response(&self, response: Result<ReleaseInfo, String>) {
        *self.create_release_response.lock().unwrap() = Some(response);
    }

    /// Configure the response for `upload_asset` calls.
    pub fn set_upload_asset_response(&self, response: Result<AssetInfo, String>) {
        *self.upload_asset_response.lock().unwrap() = Some(response);
    }

    /// Configure the response for `list_releases` calls.
    pub fn set_list_releases_response(&self, response: Result<Vec<ReleaseInfo>, String>) {
        *self.list_releases_response.lock().unwrap() = Some(response);
    }

    /// Configure the response for `delete_release` calls.
    pub fn set_delete_release_response(&self, response: Result<(), String>) {
        *self.delete_release_response.lock().unwrap() = Some(response);
    }

    // -- Call counters / accessors --

    /// Number of times `create_release` was called.
    pub fn create_release_call_count(&self) -> usize {
        self.create_release_calls.lock().unwrap().len()
    }

    /// Number of times `upload_asset` was called.
    pub fn upload_asset_call_count(&self) -> usize {
        self.upload_asset_calls.lock().unwrap().len()
    }

    /// Number of times `list_releases` was called.
    pub fn list_releases_call_count(&self) -> usize {
        self.list_releases_calls.lock().unwrap().len()
    }

    /// Number of times `delete_release` was called.
    pub fn delete_release_call_count(&self) -> usize {
        self.delete_release_calls.lock().unwrap().len()
    }

    /// Get a clone of all recorded `create_release` call parameters.
    pub fn create_release_calls(&self) -> Vec<CreateReleaseParams> {
        self.create_release_calls.lock().unwrap().clone()
    }

    /// Get a clone of all recorded `upload_asset` call parameters.
    pub fn upload_asset_calls(&self) -> Vec<UploadAssetParams> {
        self.upload_asset_calls.lock().unwrap().clone()
    }

    /// Get a clone of all recorded `list_releases` call parameters.
    pub fn list_releases_calls(&self) -> Vec<ListReleasesParams> {
        self.list_releases_calls.lock().unwrap().clone()
    }

    /// Get a clone of all recorded `delete_release` call parameters.
    pub fn delete_release_calls(&self) -> Vec<DeleteReleaseParams> {
        self.delete_release_calls.lock().unwrap().clone()
    }
}

#[cfg(feature = "test-helpers")]
impl Default for MockGitHubClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "test-helpers")]
impl GitHubClient for MockGitHubClient {
    fn create_release(&self, params: &CreateReleaseParams) -> anyhow::Result<ReleaseInfo> {
        self.create_release_calls
            .lock()
            .unwrap()
            .push(params.clone());

        match self.create_release_response.lock().unwrap().as_ref() {
            Some(Ok(info)) => Ok(info.clone()),
            Some(Err(msg)) => Err(anyhow::anyhow!("{}", msg)),
            None => Err(anyhow::anyhow!(
                "MockGitHubClient: no create_release response configured"
            )),
        }
    }

    fn upload_asset(&self, params: &UploadAssetParams) -> anyhow::Result<AssetInfo> {
        self.upload_asset_calls.lock().unwrap().push(params.clone());

        match self.upload_asset_response.lock().unwrap().as_ref() {
            Some(Ok(info)) => Ok(info.clone()),
            Some(Err(msg)) => Err(anyhow::anyhow!("{}", msg)),
            None => Err(anyhow::anyhow!(
                "MockGitHubClient: no upload_asset response configured"
            )),
        }
    }

    fn list_releases(&self, params: &ListReleasesParams) -> anyhow::Result<Vec<ReleaseInfo>> {
        self.list_releases_calls
            .lock()
            .unwrap()
            .push(params.clone());

        match self.list_releases_response.lock().unwrap().as_ref() {
            Some(Ok(releases)) => Ok(releases.clone()),
            Some(Err(msg)) => Err(anyhow::anyhow!("{}", msg)),
            None => Err(anyhow::anyhow!(
                "MockGitHubClient: no list_releases response configured"
            )),
        }
    }

    fn delete_release(&self, params: &DeleteReleaseParams) -> anyhow::Result<()> {
        self.delete_release_calls
            .lock()
            .unwrap()
            .push(params.clone());

        match self.delete_release_response.lock().unwrap().as_ref() {
            Some(Ok(())) => Ok(()),
            Some(Err(msg)) => Err(anyhow::anyhow!("{}", msg)),
            None => Err(anyhow::anyhow!(
                "MockGitHubClient: no delete_release response configured"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "test-helpers"))]
mod tests {
    use super::*;

    #[test]
    fn test_mock_records_create_release_calls() {
        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Ok(ReleaseInfo {
            id: 42,
            html_url: "https://github.com/owner/repo/releases/42".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: Some("Release v1.0.0".to_string()),
            draft: false,
        }));

        let params = CreateReleaseParams {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release v1.0.0".to_string(),
            body: "Changelog here".to_string(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        let result = mock.create_release(&params).unwrap();
        assert_eq!(result.id, 42);
        assert_eq!(result.tag_name, "v1.0.0");
        assert_eq!(mock.create_release_call_count(), 1);

        let calls = mock.create_release_calls();
        assert_eq!(calls[0].owner, "owner");
        assert_eq!(calls[0].tag_name, "v1.0.0");
    }

    #[test]
    fn test_mock_records_upload_asset_calls() {
        let mock = MockGitHubClient::new();
        mock.set_upload_asset_response(Ok(AssetInfo {
            id: 100,
            name: "myapp-linux-amd64.tar.gz".to_string(),
            size: 4096,
        }));

        let params = UploadAssetParams {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            release_id: 42,
            file_name: "myapp-linux-amd64.tar.gz".to_string(),
            file_path: PathBuf::from("/tmp/myapp-linux-amd64.tar.gz"),
        };

        let result = mock.upload_asset(&params).unwrap();
        assert_eq!(result.name, "myapp-linux-amd64.tar.gz");
        assert_eq!(mock.upload_asset_call_count(), 1);
    }

    #[test]
    fn test_mock_records_list_releases_calls() {
        let mock = MockGitHubClient::new();
        mock.set_list_releases_response(Ok(vec![ReleaseInfo {
            id: 1,
            html_url: "https://github.com/owner/repo/releases/1".to_string(),
            tag_name: "v0.9.0".to_string(),
            name: Some("Release v0.9.0".to_string()),
            draft: false,
        }]));

        let params = ListReleasesParams {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
        };

        let result = mock.list_releases(&params).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(mock.list_releases_call_count(), 1);
    }

    #[test]
    fn test_mock_records_delete_release_calls() {
        let mock = MockGitHubClient::new();
        mock.set_delete_release_response(Ok(()));

        let params = DeleteReleaseParams {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            release_id: 42,
        };

        mock.delete_release(&params).unwrap();
        assert_eq!(mock.delete_release_call_count(), 1);

        let calls = mock.delete_release_calls();
        assert_eq!(calls[0].release_id, 42);
    }

    #[test]
    fn test_mock_returns_error_when_no_response_configured() {
        let mock = MockGitHubClient::new();

        let params = CreateReleaseParams {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release".to_string(),
            body: "".to_string(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        let result = mock.create_release(&params);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no create_release response configured")
        );
    }

    #[test]
    fn test_mock_returns_configured_error() {
        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Err("API rate limit exceeded".to_string()));

        let params = CreateReleaseParams {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release".to_string(),
            body: "".to_string(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        let result = mock.create_release(&params);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("API rate limit exceeded")
        );
    }

    #[test]
    fn test_mock_multiple_calls_accumulate() {
        let mock = MockGitHubClient::new();
        mock.set_delete_release_response(Ok(()));

        for i in 1..=3 {
            let params = DeleteReleaseParams {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                release_id: i,
            };
            mock.delete_release(&params).unwrap();
        }

        assert_eq!(mock.delete_release_call_count(), 3);
        let calls = mock.delete_release_calls();
        assert_eq!(calls[0].release_id, 1);
        assert_eq!(calls[1].release_id, 2);
        assert_eq!(calls[2].release_id, 3);
    }

    #[test]
    fn test_mock_default_is_same_as_new() {
        let mock = MockGitHubClient::default();
        assert_eq!(mock.create_release_call_count(), 0);
        assert_eq!(mock.upload_asset_call_count(), 0);
        assert_eq!(mock.list_releases_call_count(), 0);
        assert_eq!(mock.delete_release_call_count(), 0);
    }
}
