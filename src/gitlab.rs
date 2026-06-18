//! GitLab forge adapter — REST API v4 over async `reqwest`.
//!
//! Self-hosted GitLab (e.g. gitlab.savvifi.com) authenticates with
//! `Authorization: Bearer` (NOT `Private-Token`). Project ids are the
//! URL-encoded `group%2Fsub%2Frepo` path; file paths are URL-encoded too.
//! Auto-merge is GitLab-native merge-when-pipeline-succeeds (MWPS).

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{
    BranchOutcome, ChangeRef, ChangeState, CiStatus, FileBlob, Forge, ForgeKind, OpenedChange,
    ForgeError, ForgeResult, PipelineStatus, RepoRef,
};

/// A GitLab adapter bound to one host + token.
pub struct GitLabForge {
    host: String,
    token: String,
    http: reqwest::Client,
}

impl GitLabForge {
    /// Build an adapter for `host` (empty → "gitlab.com") with a Bearer token.
    pub fn new(host: impl Into<String>, token: impl Into<String>) -> ForgeResult<Self> {
        let mut host = host.into();
        if host.is_empty() {
            host = "gitlab.com".into();
        }
        let http = reqwest::Client::builder()
            .user_agent("fastverk-forge")
            .build()
            .context("build gitlab http client")?;
        Ok(Self {
            host,
            token: token.into(),
            http,
        })
    }

    fn host_for<'a>(&'a self, repo: &'a RepoRef) -> &'a str {
        if repo.host.is_empty() {
            self.host.as_str()
        } else {
            repo.host.as_str()
        }
    }

    /// `https://<host>/api/v4/projects/<url-encoded owner/name>`
    fn base(&self, repo: &RepoRef) -> String {
        let full = crate::repo_slug(repo);
        format!(
            "https://{}/api/v4/projects/{}",
            self.host_for(repo),
            urlencoding::encode(&full)
        )
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> ForgeResult<reqwest::Response> {
        let resp = rb
            .bearer_auth(&self.token)
            .send()
            .await
            .context("gitlab request")?;
        Ok(resp)
    }

    /// Send + require 2xx, returning the parsed JSON body.
    async fn json<T: for<'de> Deserialize<'de>>(&self, rb: reqwest::RequestBuilder) -> ForgeResult<T> {
        let resp = self.send(rb).await?;
        let status = resp.status();
        let text = resp.text().await.context("read gitlab body")?;
        if !status.is_success() {
            return Err(ForgeError::msg(format!("gitlab {status}: {}", text.trim())));
        }
        serde_json::from_str(&text)
            .with_context(|| format!("parse gitlab json: {}", truncate(&text)))
            .map_err(ForgeError::from)
    }
}

fn truncate(s: &str) -> String {
    s.chars().take(300).collect()
}

fn map_ci(status: &str) -> CiStatus {
    match status {
        "success" => CiStatus::Success,
        "failed" => CiStatus::Failed,
        "canceled" | "skipped" => CiStatus::Canceled,
        "running" => CiStatus::Running,
        "created" | "waiting_for_resource" | "preparing" | "pending" | "scheduled" | "manual" => {
            CiStatus::Pending
        }
        _ => CiStatus::Unspecified,
    }
}

#[derive(Deserialize)]
struct Project {
    default_branch: Option<String>,
}

#[derive(Deserialize)]
struct GlFile {
    content: String,             // base64
    blob_id: String,
    last_commit_id: String,
    file_path: String,
}

#[derive(Deserialize)]
struct GlMr {
    iid: u64,
    web_url: String,
    source_branch: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    merge_commit_sha: Option<String>,
}

#[derive(Deserialize)]
struct GlPipeline {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    status: String,
    #[serde(default)]
    web_url: String,
}

#[async_trait]
impl Forge for GitLabForge {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Gitlab
    }

    async fn default_branch(&self, repo: &RepoRef) -> ForgeResult<String> {
        let p: Project = self.json(self.http.get(self.base(repo))).await?;
        Ok(p.default_branch.unwrap_or_else(|| "main".into()))
    }

    async fn read_file(&self, repo: &RepoRef, path: &str, r#ref: &str) -> ForgeResult<Option<FileBlob>> {
        let branch = if r#ref.is_empty() {
            self.default_branch(repo).await?
        } else {
            r#ref.to_string()
        };
        let url = format!(
            "{}/repository/files/{}?ref={}",
            self.base(repo),
            urlencoding::encode(path),
            urlencoding::encode(&branch)
        );
        let resp = self.send(self.http.get(url)).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = resp.status();
        let text = resp.text().await.context("read gitlab file body")?;
        if !status.is_success() {
            return Err(ForgeError::msg(format!("gitlab read_file {status}: {}", text.trim())));
        }
        let f: GlFile = serde_json::from_str(&text).context("parse gitlab file")?;
        let raw = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            f.content.replace(['\n', '\r'], ""),
        )
        .context("decode gitlab file content")?;
        let content = String::from_utf8(raw).context("gitlab file not utf-8")?;
        // Carry last_commit_id as blob_sha — GitLab wants it on update.
        let _ = (f.blob_id, f.file_path);
        Ok(Some(FileBlob {
            path: path.to_string(),
            content,
            blob_sha: f.last_commit_id,
        }))
    }

    async fn create_branch(
        &self,
        repo: &RepoRef,
        name: &str,
        from_ref: &str,
    ) -> ForgeResult<BranchOutcome> {
        let url = format!(
            "{}/repository/branches?branch={}&ref={}",
            self.base(repo),
            urlencoding::encode(name),
            urlencoding::encode(from_ref)
        );
        let resp = self.send(self.http.post(url)).await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(BranchOutcome {
                created: true,
                already_existed: false,
            });
        }
        let text = resp.text().await.unwrap_or_default();
        if text.contains("already exists") {
            return Ok(BranchOutcome {
                created: false,
                already_existed: true,
            });
        }
        return Err(ForgeError::msg(format!("gitlab create_branch {status}: {}", text.trim())));
    }

    async fn commit_file(
        &self,
        repo: &RepoRef,
        branch: &str,
        path: &str,
        content: &str,
        blob_sha: &str,
        message: &str,
    ) -> ForgeResult<String> {
        let url = format!("{}/repository/files/{}", self.base(repo), urlencoding::encode(path));
        let mut body = json!({
            "branch": branch,
            "content": content,
            "commit_message": message,
        });
        if !blob_sha.is_empty() {
            body["last_commit_id"] = json!(blob_sha);
        }
        let resp = self.send(self.http.put(url).json(&body)).await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ForgeError::msg(format!("gitlab commit_file {status}: {}", text.trim())));
        }
        // GitLab's files PUT returns {file_path, branch} (no commit sha);
        // the merge commit is what downstream cares about, so return empty.
        Ok(String::new())
    }

    async fn open_change(
        &self,
        repo: &RepoRef,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        remove_source_branch: bool,
    ) -> ForgeResult<OpenedChange> {
        let url = format!("{}/merge_requests", self.base(repo));
        let payload = json!({
            "source_branch": head,
            "target_branch": base,
            "title": title,
            "description": body,
            "remove_source_branch": remove_source_branch,
        });
        let resp = self.send(self.http.post(&url).json(&payload)).await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            let mr: GlMr = serde_json::from_str(&text).context("parse opened MR")?;
            return Ok(OpenedChange {
                change: ChangeRef {
                    number: mr.iid,
                    url: mr.web_url,
                    branch: mr.source_branch,
                },
                already_existed: false,
            });
        }
        // An open MR already exists for this source branch → adopt it.
        if text.contains("already exists") {
            let list_url = format!(
                "{}/merge_requests?state=opened&source_branch={}",
                self.base(repo),
                urlencoding::encode(head)
            );
            let mrs: Vec<GlMr> = self.json(self.http.get(list_url)).await?;
            let mr = mrs
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("MR exists for {head} but none returned"))?;
            return Ok(OpenedChange {
                change: ChangeRef {
                    number: mr.iid,
                    url: mr.web_url,
                    branch: mr.source_branch,
                },
                already_existed: true,
            });
        }
        return Err(ForgeError::msg(format!("gitlab open_change {status}: {}", text.trim())));
    }

    async fn enable_auto_merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<bool> {
        let url = format!("{}/merge_requests/{}/merge", self.base(repo), change.number);
        let resp = self
            .send(
                self.http
                    .put(url)
                    .json(&json!({ "merge_when_pipeline_succeeds": true })),
            )
            .await?;
        let status = resp.status();
        // 200 = enabled (or merged immediately); 405/406 = not mergeable yet
        // (no pipeline / conflicts) — the caller falls back to polling + merge.
        if status.is_success() {
            return Ok(true);
        }
        if status == reqwest::StatusCode::METHOD_NOT_ALLOWED
            || status == reqwest::StatusCode::NOT_ACCEPTABLE
        {
            return Ok(false);
        }
        let text = resp.text().await.unwrap_or_default();
        return Err(ForgeError::msg(format!("gitlab enable_auto_merge {status}: {}", text.trim())));
    }

    async fn pipeline_status(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<PipelineStatus> {
        let url = format!(
            "{}/merge_requests/{}/pipelines",
            self.base(repo),
            change.number
        );
        let pipelines: Vec<GlPipeline> = self.json(self.http.get(url)).await?;
        match pipelines.into_iter().next() {
            Some(p) => Ok(PipelineStatus {
                status: map_ci(&p.status),
                pipeline_id: p.id.to_string(),
                url: p.web_url,
            }),
            None => Ok(PipelineStatus {
                status: CiStatus::None,
                pipeline_id: String::new(),
                url: String::new(),
            }),
        }
    }

    async fn merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<String> {
        let url = format!("{}/merge_requests/{}/merge", self.base(repo), change.number);
        let mr: GlMr = self.json(self.http.put(url)).await?;
        Ok(mr.merge_commit_sha.unwrap_or_default())
    }

    async fn change_state(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<ChangeState> {
        let url = format!("{}/merge_requests/{}", self.base(repo), change.number);
        let mr: GlMr = self.json(self.http.get(url)).await?;
        Ok(match mr.state.as_str() {
            "merged" => ChangeState::Merged,
            "closed" => ChangeState::Closed,
            _ => ChangeState::Open,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_status_mapping() {
        assert_eq!(map_ci("success"), CiStatus::Success);
        assert_eq!(map_ci("failed"), CiStatus::Failed);
        assert_eq!(map_ci("running"), CiStatus::Running);
        assert_eq!(map_ci("pending"), CiStatus::Pending);
        assert_eq!(map_ci("canceled"), CiStatus::Canceled);
        assert_eq!(map_ci("skipped"), CiStatus::Canceled);
        assert_eq!(map_ci("weird"), CiStatus::Unspecified);
    }

    #[test]
    fn nested_group_project_id_is_url_encoded() {
        let f = GitLabForge::new("gitlab.savvifi.com", "tok").unwrap();
        let repo = RepoRef {
            forge: ForgeKind::Gitlab as i32,
            host: String::new(),
            owner: "studio".into(),
            name: "web".into(),
        };
        assert_eq!(
            f.base(&repo),
            "https://gitlab.savvifi.com/api/v4/projects/studio%2Fweb"
        );
        let nested = RepoRef {
            owner: "group/sub".into(),
            name: "repo".into(),
            ..repo
        };
        assert_eq!(
            f.base(&nested),
            "https://gitlab.savvifi.com/api/v4/projects/group%2Fsub%2Frepo"
        );
    }
}
