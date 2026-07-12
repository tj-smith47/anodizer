//! Minimal GitHub REST surface for the homebrew-core bump: read a formula
//! file, create a bump branch, commit the rewritten formula through the
//! contents API, ensure a fork, and open the pull request. Everything is
//! API-only — the publisher never clones the (multi-gigabyte) formula repo.
//!
//! The API base resolves through [`anodizer_core::http::github_api_base`],
//! so tests drive the whole flow against an in-process scripted responder
//! via the `ANODIZER_GITHUB_API_BASE` override.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;

/// One authenticated GitHub API session (blocking).
pub(crate) struct GithubApi {
    client: reqwest::blocking::Client,
    base: String,
    token: String,
}

/// A formula file as read from the contents API.
pub(crate) struct RepoFile {
    /// Decoded UTF-8 file content.
    pub content: String,
    /// Git blob SHA — required by the contents API to update the file.
    pub sha: String,
    /// Repo-relative path the file was found at.
    pub path: String,
}

/// Repository facts consulted before choosing the commit path.
pub(crate) struct RepoInfo {
    pub default_branch: String,
    /// Whether the token can push to this repository (`permissions.push`).
    pub can_push: bool,
}

/// Outcome of a `POST /pulls` attempt.
pub(crate) enum PrOutcome {
    /// PR created; carries `(number, html_url)`.
    Created(u64, String),
    /// GitHub rejected the create with the already-exists 422 — an open PR
    /// with the same head/base is live from an earlier run.
    AlreadyExists,
}

impl GithubApi {
    pub(crate) fn new<E: anodizer_core::EnvSource + ?Sized>(env: &E, token: &str) -> Result<Self> {
        Ok(Self {
            client: anodizer_core::http::blocking_client(Duration::from_secs(30))
                .context("homebrew-core: build HTTP client")?,
            base: anodizer_core::http::github_api_base(env),
            token: token.to_string(),
        })
    }

    fn get(&self, path: &str) -> reqwest::blocking::RequestBuilder {
        self.request(reqwest::Method::GET, path)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        let mut req = self
            .client
            .request(method, format!("{}{}", self.base, path))
            .header("Accept", "application/vnd.github+json");
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        req
    }

    /// `GET /repos/{owner}/{repo}` — default branch + push permission.
    pub(crate) fn repo_info(&self, owner: &str, repo: &str) -> Result<RepoInfo> {
        let path = format!("/repos/{}/{}", owner, repo);
        let resp = self
            .get(&path)
            .send()
            .with_context(|| format!("homebrew-core: GET {}", path))?;
        let status = resp.status();
        if !status.is_success() {
            bail!(
                "homebrew-core: GET {} returned HTTP {}: {}",
                path,
                status,
                anodizer_core::http::body_of_blocking(resp)
            );
        }
        let body: serde_json::Value = resp.json().context("homebrew-core: parse repo JSON")?;
        Ok(RepoInfo {
            default_branch: body
                .get("default_branch")
                .and_then(|v| v.as_str())
                .unwrap_or("main")
                .to_string(),
            can_push: body
                .pointer("/permissions/push")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }

    /// `GET /repos/{owner}/{repo}/contents/{path}?ref={branch}` — `Ok(None)`
    /// on 404 so the caller can fall through the sharded → flat path probe.
    pub(crate) fn get_file(
        &self,
        owner: &str,
        repo: &str,
        file_path: &str,
        r#ref: &str,
    ) -> Result<Option<RepoFile>> {
        let path = format!(
            "/repos/{}/{}/contents/{}?ref={}",
            owner, repo, file_path, r#ref
        );
        let resp = self
            .get(&path)
            .send()
            .with_context(|| format!("homebrew-core: GET {}", path))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            bail!(
                "homebrew-core: GET {} returned HTTP {}: {}",
                path,
                status,
                anodizer_core::http::body_of_blocking(resp)
            );
        }
        let body: serde_json::Value = resp.json().context("homebrew-core: parse contents JSON")?;
        let sha = body
            .get("sha")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // The contents API base64-encodes with line wrapping; strip all
        // whitespace before decoding.
        let raw: String = body
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(raw)
            .context("homebrew-core: base64-decode formula content")?;
        Ok(Some(RepoFile {
            content: String::from_utf8(bytes)
                .context("homebrew-core: formula content is not UTF-8")?,
            sha,
            path: file_path.to_string(),
        }))
    }

    /// `GET /repos/{owner}/{repo}/git/ref/heads/{branch}` — the commit SHA a
    /// branch points at.
    pub(crate) fn branch_sha(&self, owner: &str, repo: &str, branch: &str) -> Result<String> {
        let path = format!("/repos/{}/{}/git/ref/heads/{}", owner, repo, branch);
        let resp = self
            .get(&path)
            .send()
            .with_context(|| format!("homebrew-core: GET {}", path))?;
        let status = resp.status();
        if !status.is_success() {
            bail!(
                "homebrew-core: GET {} returned HTTP {}: {}",
                path,
                status,
                anodizer_core::http::body_of_blocking(resp)
            );
        }
        let body: serde_json::Value = resp.json().context("homebrew-core: parse ref JSON")?;
        body.pointer("/object/sha")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("homebrew-core: ref response for {} has no sha", path))
    }

    /// `POST /repos/{owner}/{repo}/git/refs` — create the bump branch at
    /// `sha`. When the branch already exists (a re-run), it is force-moved
    /// to `sha` instead so the retried bump starts from the fresh base.
    pub(crate) fn create_or_reset_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        sha: &str,
    ) -> Result<()> {
        let path = format!("/repos/{}/{}/git/refs", owner, repo);
        let resp = self
            .request(reqwest::Method::POST, &path)
            .json(&serde_json::json!({
                "ref": format!("refs/heads/{}", branch),
                "sha": sha,
            }))
            .send()
            .with_context(|| format!("homebrew-core: POST {}", path))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = anodizer_core::http::body_of_blocking(resp);
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY && body.contains("already exists") {
            let patch_path = format!("/repos/{}/{}/git/refs/heads/{}", owner, repo, branch);
            let resp = self
                .request(reqwest::Method::PATCH, &patch_path)
                .json(&serde_json::json!({ "sha": sha, "force": true }))
                .send()
                .with_context(|| format!("homebrew-core: PATCH {}", patch_path))?;
            let status = resp.status();
            if !status.is_success() {
                bail!(
                    "homebrew-core: PATCH {} returned HTTP {}: {}",
                    patch_path,
                    status,
                    anodizer_core::http::body_of_blocking(resp)
                );
            }
            return Ok(());
        }
        bail!(
            "homebrew-core: POST {} returned HTTP {}: {}",
            path,
            status,
            body
        );
    }

    /// `PUT /repos/{owner}/{repo}/contents/{path}` — commit the rewritten
    /// formula to `branch`. `prev_sha` is the blob SHA read by
    /// [`Self::get_file`]; the API rejects the update if the file changed
    /// underneath us (a concurrent bump), which is exactly the safe failure.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn put_file(
        &self,
        owner: &str,
        repo: &str,
        file_path: &str,
        branch: &str,
        message: &str,
        content: &str,
        prev_sha: &str,
    ) -> Result<()> {
        let path = format!("/repos/{}/{}/contents/{}", owner, repo, file_path);
        let resp = self
            .request(reqwest::Method::PUT, &path)
            .json(&serde_json::json!({
                "message": message,
                "content": base64::engine::general_purpose::STANDARD.encode(content),
                "sha": prev_sha,
                "branch": branch,
            }))
            .send()
            .with_context(|| format!("homebrew-core: PUT {}", path))?;
        let status = resp.status();
        if !status.is_success() {
            bail!(
                "homebrew-core: PUT {} returned HTTP {}: {}",
                path,
                status,
                anodizer_core::http::body_of_blocking(resp)
            );
        }
        Ok(())
    }

    /// `POST /repos/{owner}/{repo}/forks` — create (or fetch the existing)
    /// fork for the authenticated user. Returns the fork owner's login.
    /// GitHub returns 202 with the existing fork when one is already there,
    /// so this is naturally idempotent. Fork creation is asynchronous, but
    /// the bump only needs the ref namespace — a fork shares its parent's
    /// object store, so the upstream base SHA is immediately usable.
    pub(crate) fn ensure_fork(&self, owner: &str, repo: &str) -> Result<String> {
        let path = format!("/repos/{}/{}/forks", owner, repo);
        let resp = self
            .request(reqwest::Method::POST, &path)
            .json(&serde_json::json!({}))
            .send()
            .with_context(|| format!("homebrew-core: POST {}", path))?;
        let status = resp.status();
        if !status.is_success() {
            bail!(
                "homebrew-core: POST {} returned HTTP {}: {}",
                path,
                status,
                anodizer_core::http::body_of_blocking(resp)
            );
        }
        let body: serde_json::Value = resp.json().context("homebrew-core: parse fork JSON")?;
        body.pointer("/owner/login")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("homebrew-core: fork response has no owner login"))
    }

    /// `POST /repos/{owner}/{repo}/pulls` — open the bump PR. The
    /// already-exists 422 folds to [`PrOutcome::AlreadyExists`] so the
    /// caller can surface the idempotent skip.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_pr(
        &self,
        upstream_owner: &str,
        upstream_repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
        draft: bool,
    ) -> Result<PrOutcome> {
        let path = format!("/repos/{}/{}/pulls", upstream_owner, upstream_repo);
        let resp = self
            .request(reqwest::Method::POST, &path)
            .json(&serde_json::json!({
                "title": title,
                "body": body,
                "head": head,
                "base": base,
                "draft": draft,
            }))
            .send()
            .with_context(|| format!("homebrew-core: POST {}", path))?;
        let status = resp.status();
        if status.is_success() {
            let body: serde_json::Value = resp.json().context("homebrew-core: parse PR JSON")?;
            let number = body.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
            let url = body
                .get("html_url")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            return Ok(PrOutcome::Created(number, url));
        }
        let body = anodizer_core::http::body_of_blocking(resp);
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY && body.contains("already exists") {
            return Ok(PrOutcome::AlreadyExists);
        }
        bail!(
            "homebrew-core: POST {} returned HTTP {}: {}",
            path,
            status,
            body
        );
    }
}

/// Download `url` and return the hex SHA-256 of the body — the digest
/// written into the formula when no `sha256:` override is configured. The
/// release tarball must already be live (the release/publish ordering
/// guarantees the GitHub tag exists by the time publishers run).
pub(crate) fn download_sha256(url: &str) -> Result<String> {
    use sha2::Digest as _;
    let client = anodizer_core::http::blocking_client(Duration::from_secs(300))
        .context("homebrew-core: build download client")?;
    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("homebrew-core: download {}", url))?;
    let status = resp.status();
    if !status.is_success() {
        bail!(
            "homebrew-core: download {} returned HTTP {}: {}",
            url,
            status,
            anodizer_core::http::body_of_blocking(resp)
        );
    }
    let bytes = resp
        .bytes()
        .with_context(|| format!("homebrew-core: read download body from {}", url))?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}
