//! GitHub forge adapter — octocrab + raw REST/GraphQL.
//!
//! The branch/commit/PR flow is lifted from the original
//! `services/planning/src/wave.rs`. Auto-merge uses the GraphQL
//! `enablePullRequestAutoMerge` mutation (REST cannot enable it); pipeline
//! status reads the head sha's check-runs (falling back to the combined
//! status API). github.com only for now (Enterprise = a `base_uri` follow-up).

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use base64::Engine as _;
use octocrab::Octocrab;
use serde_json::{json, Value};

use crate::{
    default_trigger_events, BranchOutcome, ChangeRef, ChangeState, CiStatus, EnsuredTrigger,
    FileBlob, Forge, ForgeCapabilities, ForgeError, ForgeKind, ForgeResult, OpenedChange,
    PipelineStatus, RepoRef, Trigger,
};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// A GitHub adapter wrapping an authenticated octocrab client.
pub struct GitHubForge {
    client: Octocrab,
}

impl GitHubForge {
    /// Build from a personal-access token.
    pub fn new(token: impl Into<String>) -> ForgeResult<Self> {
        let client = Octocrab::builder()
            .personal_token(token.into())
            .build()
            .context("build octocrab client")?;
        Ok(Self { client })
    }

    /// Build from an existing octocrab client.
    #[must_use]
    pub fn from_client(client: Octocrab) -> Self {
        Self { client }
    }

    fn owner(repo: &RepoRef) -> &str {
        &repo.owner
    }

    /// Fetch the raw PR JSON (reused for state/head-sha/node-id).
    async fn pr_json(&self, repo: &RepoRef, number: u64) -> ForgeResult<Value> {
        let route = format!("/repos/{}/{}/pulls/{}", repo.owner, repo.name, number);
        self.client
            .get::<Value, _, ()>(&route, None)
            .await
            .with_context(|| format!("get PR {route}"))
            .map_err(ForgeError::from)
    }
}

fn map_check_conclusion(conclusion: Option<&str>) -> Option<CiStatus> {
    match conclusion {
        Some("success") | Some("neutral") | Some("skipped") => Some(CiStatus::Success),
        Some("failure") | Some("timed_out") | Some("action_required") | Some("startup_failure") => {
            Some(CiStatus::Failed)
        }
        Some("cancelled") => Some(CiStatus::Canceled),
        _ => None,
    }
}

#[async_trait]
impl Forge for GitHubForge {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Github
    }

    async fn default_branch(&self, repo: &RepoRef) -> ForgeResult<String> {
        let meta = self
            .client
            .repos(Self::owner(repo), &repo.name)
            .get()
            .await
            .with_context(|| format!("get repo {}/{}", repo.owner, repo.name))?;
        Ok(meta.default_branch.unwrap_or_else(|| "main".into()))
    }

    async fn read_file(
        &self,
        repo: &RepoRef,
        path: &str,
        r#ref: &str,
    ) -> ForgeResult<Option<FileBlob>> {
        let branch = if r#ref.is_empty() {
            self.default_branch(repo).await?
        } else {
            r#ref.to_string()
        };
        let res = self
            .client
            .repos(Self::owner(repo), &repo.name)
            .get_content()
            .path(path)
            .r#ref(&branch)
            .send()
            .await;
        let contents = match res {
            Ok(c) => c,
            Err(octocrab::Error::GitHub { source, .. })
                if source.status_code == http::StatusCode::NOT_FOUND =>
            {
                return Ok(None);
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("get content {path}"))
                    .map_err(ForgeError::from)
            }
        };
        let Some(item) = contents.items.into_iter().next() else {
            return Ok(None);
        };
        let content = item
            .decoded_content()
            .ok_or_else(|| anyhow!("{path}: base64 decode failed"))?;
        Ok(Some(FileBlob {
            path: path.to_string(),
            content,
            blob_sha: item.sha,
        }))
    }

    async fn create_branch(
        &self,
        repo: &RepoRef,
        name: &str,
        from_ref: &str,
    ) -> ForgeResult<BranchOutcome> {
        // Resolve from_ref → commit sha (it may be a branch name or a sha).
        let base_ref = self
            .client
            .repos(Self::owner(repo), &repo.name)
            .get_ref(&octocrab::params::repos::Reference::Branch(
                from_ref.to_string(),
            ))
            .await
            .with_context(|| format!("get_ref {from_ref}"))?;
        let base_sha = match &base_ref.object {
            octocrab::models::repos::Object::Commit { sha, .. } => sha.clone(),
            other => return Err(ForgeError::msg(format!("unexpected ref object: {other:?}"))),
        };
        let route = format!("/repos/{}/{}/git/refs", repo.owner, repo.name);
        let body = json!({ "ref": format!("refs/heads/{name}"), "sha": base_sha });
        match self.client.post::<_, Value>(&route, Some(&body)).await {
            Ok(_) => Ok(BranchOutcome {
                created: true,
                already_existed: false,
            }),
            Err(e) => {
                // Idempotent: if the branch now exists, treat as already-existed.
                let exists = self
                    .client
                    .repos(Self::owner(repo), &repo.name)
                    .get_ref(&octocrab::params::repos::Reference::Branch(
                        name.to_string(),
                    ))
                    .await
                    .is_ok();
                if exists {
                    Ok(BranchOutcome {
                        created: false,
                        already_existed: true,
                    })
                } else {
                    Err(e).context("create branch").map_err(ForgeError::from)
                }
            }
        }
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
        let route = format!("/repos/{}/{}/contents/{}", repo.owner, repo.name, path);
        let mut body = json!({
            "message": message,
            "content": B64.encode(content.as_bytes()),
            "branch": branch,
        });
        if !blob_sha.is_empty() {
            body["sha"] = json!(blob_sha);
        }
        let resp: Value = self
            .client
            .put(&route, Some(&body))
            .await
            .context("commit file")?;
        Ok(resp
            .get("commit")
            .and_then(|c| c.get("sha"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string())
    }

    async fn open_change(
        &self,
        repo: &RepoRef,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        _remove_source_branch: bool,
    ) -> ForgeResult<OpenedChange> {
        match self
            .client
            .pulls(Self::owner(repo), &repo.name)
            .create(title, head, base)
            .body(body)
            .send()
            .await
        {
            Ok(pr) => Ok(OpenedChange {
                change: ChangeRef {
                    number: pr.number,
                    url: pr.html_url.map(|u| u.to_string()).unwrap_or_default(),
                    branch: head.to_string(),
                    // Numeric forge: `number` is authoritative.
                    id: String::new(),
                },
                already_existed: false,
            }),
            Err(e) => {
                // Adopt an existing open PR for this head branch.
                let existing = self
                    .client
                    .pulls(Self::owner(repo), &repo.name)
                    .list()
                    .head(format!("{}:{}", repo.owner, head))
                    .state(octocrab::params::State::Open)
                    .send()
                    .await
                    .ok()
                    .and_then(|page| page.items.into_iter().next());
                if let Some(pr) = existing {
                    Ok(OpenedChange {
                        change: ChangeRef {
                            number: pr.number,
                            url: pr.html_url.map(|u| u.to_string()).unwrap_or_default(),
                            branch: head.to_string(),
                            // Numeric forge: `number` is authoritative.
                            id: String::new(),
                        },
                        already_existed: true,
                    })
                } else {
                    Err(e).context("open PR").map_err(ForgeError::from)
                }
            }
        }
    }

    async fn enable_auto_merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<bool> {
        let pr = self.pr_json(repo, change.number).await?;
        let Some(node_id) = pr.get("node_id").and_then(|v| v.as_str()) else {
            return Ok(false);
        };
        let query = r"mutation($id:ID!){enablePullRequestAutoMerge(input:{pullRequestId:$id,mergeMethod:MERGE}){clientMutationId}}";
        let body = json!({ "query": query, "variables": { "id": node_id } });
        match self.client.graphql::<Value>(&body).await {
            Ok(v) if v.get("errors").is_none() => Ok(true),
            // auto-merge disabled on the repo / not allowed → caller falls back.
            _ => Ok(false),
        }
    }

    async fn pipeline_status(
        &self,
        repo: &RepoRef,
        change: &ChangeRef,
    ) -> ForgeResult<PipelineStatus> {
        let pr = self.pr_json(repo, change.number).await?;
        let head_sha = pr
            .get("head")
            .and_then(|h| h.get("sha"))
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow!("PR {} has no head.sha", change.number))?;

        let route = format!(
            "/repos/{}/{}/commits/{}/check-runs",
            repo.owner, repo.name, head_sha
        );
        let runs: Value = self
            .client
            .get(&route, None::<&()>)
            .await
            .context("check-runs")?;
        let arr = runs.get("check_runs").and_then(|v| v.as_array());
        let url = format!(
            "https://github.com/{}/{}/commits/{}",
            repo.owner, repo.name, head_sha
        );

        if let Some(runs) = arr {
            if !runs.is_empty() {
                let mut any_running = false;
                let mut any_failed = false;
                for r in runs {
                    let status = r.get("status").and_then(|v| v.as_str());
                    if status != Some("completed") {
                        any_running = true;
                        continue;
                    }
                    match map_check_conclusion(r.get("conclusion").and_then(|v| v.as_str())) {
                        Some(CiStatus::Failed) => any_failed = true,
                        Some(CiStatus::Canceled) => any_failed = true,
                        _ => {}
                    }
                }
                let status = if any_failed {
                    CiStatus::Failed
                } else if any_running {
                    CiStatus::Running
                } else {
                    CiStatus::Success
                };
                return Ok(PipelineStatus {
                    status,
                    pipeline_id: head_sha.to_string(),
                    url,
                });
            }
        }

        // Fall back to the legacy combined status API.
        let sroute = format!(
            "/repos/{}/{}/commits/{}/status",
            repo.owner, repo.name, head_sha
        );
        let combined: Value = self
            .client
            .get(&sroute, None::<&()>)
            .await
            .context("combined status")?;
        let status = match combined.get("state").and_then(|v| v.as_str()) {
            Some("success") => CiStatus::Success,
            Some("failure") | Some("error") => CiStatus::Failed,
            Some("pending") => CiStatus::Running,
            _ => CiStatus::None,
        };
        Ok(PipelineStatus {
            status,
            pipeline_id: head_sha.to_string(),
            url,
        })
    }

    async fn merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<String> {
        let route = format!(
            "/repos/{}/{}/pulls/{}/merge",
            repo.owner, repo.name, change.number
        );
        let resp: Value = self
            .client
            .put(&route, Some(&json!({ "merge_method": "merge" })))
            .await
            .context("merge PR")?;
        Ok(resp
            .get("sha")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string())
    }

    async fn change_state(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<ChangeState> {
        let pr = self.pr_json(repo, change.number).await?;
        let merged = pr.get("merged").and_then(Value::as_bool).unwrap_or(false);
        if merged {
            return Ok(ChangeState::Merged);
        }
        Ok(match pr.get("state").and_then(|v| v.as_str()) {
            Some("closed") => ChangeState::Closed,
            _ => ChangeState::Open,
        })
    }

    async fn list_triggers(&self, repo: &RepoRef) -> ForgeResult<Vec<Trigger>> {
        let route = format!("/repos/{}/{}/hooks", repo.owner, repo.name);
        let hooks: Vec<Value> = self
            .client
            .get::<Vec<Value>, _, ()>(&route, None)
            .await
            .with_context(|| format!("list hooks {route}"))?;
        Ok(hooks.iter().map(gh_hook_to_trigger).collect())
    }

    async fn ensure_trigger(
        &self,
        repo: &RepoRef,
        url: &str,
        events: &[String],
        secret: &str,
    ) -> ForgeResult<EnsuredTrigger> {
        // Idempotent: a hook already delivering to `url` is returned as-is.
        if let Some(trigger) = self
            .list_triggers(repo)
            .await?
            .into_iter()
            .find(|t| t.url == url)
        {
            return Ok(EnsuredTrigger {
                trigger,
                created: false,
            });
        }
        let events = if events.is_empty() {
            default_trigger_events()
        } else {
            events.to_vec()
        };
        let mut config = json!({ "url": url, "content_type": "json" });
        if !secret.is_empty() {
            config["secret"] = json!(secret);
        }
        let body = json!({ "name": "web", "active": true, "events": events, "config": config });
        let route = format!("/repos/{}/{}/hooks", repo.owner, repo.name);
        let created: Value = self
            .client
            .post::<_, Value>(&route, Some(&body))
            .await
            .with_context(|| format!("create hook {route}"))?;
        Ok(EnsuredTrigger {
            trigger: gh_hook_to_trigger(&created),
            created: true,
        })
    }

    /// Core + inbound triggers, and nothing else — an honest report of what THIS
    /// adapter serves, not of what GitHub offers.
    ///
    /// GitHub does have issues, checks, deployments and the rest; this
    /// adapter has not wired them (the implementations live in plugin-forge's
    /// backend, which routes by configuration rather than per request). A caller
    /// gating on this therefore skips surfaces GitHub could serve — the safe
    /// direction. Each flag flips true in the commit that implements its method,
    /// never before, or the flag becomes a promise the adapter cannot keep.
    async fn capabilities(&self) -> ForgeResult<ForgeCapabilities> {
        Ok(ForgeCapabilities {
            triggers: true,
            ..Default::default()
        })
    }
}

/// Map a GitHub hook JSON object to a normalized [`Trigger`]. GitHub already
/// uses "push"/"pull_request" event names, so events pass through.
fn gh_hook_to_trigger(h: &Value) -> Trigger {
    Trigger {
        // hook id is a JSON number → stringify (Value::to_string on a number
        // yields the bare digits, e.g. "12345").
        id: h.get("id").map(ToString::to_string).unwrap_or_default(),
        url: h
            .get("config")
            .and_then(|c| c.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        events: h
            .get("events")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        active: h.get("active").and_then(Value::as_bool).unwrap_or(false),
        // Delivers by HTTPS, so `url` carries it.
        target: String::new(),
    }
}

#[cfg(test)]
mod trigger_tests {
    use super::*;

    #[test]
    fn maps_github_hook_to_trigger() {
        let hook = json!({
            "id": 12345,
            "active": true,
            "events": ["push", "pull_request"],
            "config": { "url": "https://hooks.fastverk.com/webhook", "content_type": "json" }
        });
        let t = gh_hook_to_trigger(&hook);
        assert_eq!(t.id, "12345");
        assert_eq!(t.url, "https://hooks.fastverk.com/webhook");
        assert_eq!(
            t.events,
            vec!["push".to_string(), "pull_request".to_string()]
        );
        assert!(t.active);
    }
}
