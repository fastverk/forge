//! Runtime pinning — run a [`Forge`] adapter's HTTP on a runtime this crate owns.
//!
//! # The bug this exists for
//!
//! Under Bazel every module resolves its dependencies through its OWN
//! `crate_universe`. So when another module depends on `forge`, this crate's
//! reqwest/octocrab/hyper link against **forge's** tokio while the consumer
//! awaits on **its own**. Two distinct tokio crates mean two distinct reactor
//! thread-locals, and the first DNS resolution panics:
//!
//! ```text
//! thread 'main' panicked at
//!   external/…+crate+forge+0.0.1+crate+crates__hyper-util-0.1.20/src/client/legacy/connect/dns.rs
//! there is no reactor running, must be called from the context of a Tokio 1.x runtime
//! ```
//!
//! This is not hypothetical. It is why the `wave-discover-*` CronJobs have
//! failed on **every** run — `wave` builds a `GitHubForge`/`GitLabForge` and
//! drives it from wave's runtime. The identical failure took down the first live
//! backlog dispatch in the sibling `tracker` module.
//!
//! Note how differently it presents depending on which thread loses: `wave` dies
//! on `main`, so the Job fails visibly; an adapter driven from a server's worker
//! thread leaves the process **Running with zero restarts** and the caller sees
//! only a cancelled stream. The second shape is the dangerous one.
//!
//! # The fix
//!
//! [`pin`] wraps any adapter so each call is spawned onto a runtime this crate
//! owns and the caller awaits the `JoinHandle` — which needs no reactor of its
//! own, being a completion channel. The reactor hyper needs is then always the
//! one it was compiled against.
//!
//! ```rust,ignore
//! let f = forge::runtime::pin(GitHubForge::new(token)?);
//! ```
//!
//! An in-process consumer that shares this crate's tokio (anything built in the
//! same module) does not need it, and paying for it is harmless either way — the
//! cost is two background threads per process.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tokio::runtime::{Builder, Runtime};

use crate::{
    BranchOutcome, ChangeRef, ChangeState, EnsuredTrigger, FileBlob, Forge, ForgeCapabilities,
    ForgeError, ForgeKind, ForgeResult, Issue, OpenedChange, PipelineStatus, PullRequest, RepoRef,
    Repository, Trigger,
};

/// The runtime every pinned call runs on. Owned by this crate, so the reactor
/// matches the hyper that was compiled with it.
fn http_rt() -> &'static Runtime {
    static HTTP_RT: OnceLock<Runtime> = OnceLock::new();
    HTTP_RT.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("forge-http")
            .build()
            .expect("build the forge HTTP runtime")
    })
}

/// Wrap `inner` so its calls run on this crate's runtime. See the module docs.
pub fn pin<F: Forge + 'static>(inner: F) -> Pinned {
    Pinned {
        inner: Arc::new(inner),
    }
}

/// Wrap an already-boxed adapter — the shape a factory returns.
pub fn pin_boxed(inner: Box<dyn Forge>) -> Pinned {
    Pinned {
        inner: Arc::from(inner),
    }
}

/// Wrap a shared adapter, keeping the caller's handle to the concrete type.
///
/// Used by the conformance fixture, which needs the underlying `FakeForge` for
/// fault injection while driving the pinned wrapper.
pub fn pin_arc(inner: Arc<dyn Forge>) -> Pinned {
    Pinned { inner }
}

/// A [`Forge`] whose every call is executed on this crate's runtime.
pub struct Pinned {
    inner: Arc<dyn Forge>,
}

/// Spawn `body` onto the owned runtime and flatten the join error.
///
/// A `JoinError` here means the task panicked or was cancelled; surfacing it as
/// a `ForgeError` keeps a panic inside an adapter from becoming a silent hang.
macro_rules! on_rt {
    ($body:expr) => {
        http_rt()
            .spawn($body)
            .await
            .map_err(|e| ForgeError::msg(format!("forge call failed to join: {e}")))?
    };
}

#[async_trait]
impl Forge for Pinned {
    fn kind(&self) -> ForgeKind {
        // Not async and does no I/O — no reason to cross the runtime.
        self.inner.kind()
    }

    async fn default_branch(&self, repo: &RepoRef) -> ForgeResult<String> {
        let (in_, repo) = (self.inner.clone(), repo.clone());
        on_rt!(async move { in_.default_branch(&repo).await })
    }

    async fn read_file(
        &self,
        repo: &RepoRef,
        path: &str,
        r#ref: &str,
    ) -> ForgeResult<Option<FileBlob>> {
        let (in_, repo, path, r) = (
            self.inner.clone(),
            repo.clone(),
            path.to_string(),
            r#ref.to_string(),
        );
        on_rt!(async move { in_.read_file(&repo, &path, &r).await })
    }

    async fn create_branch(
        &self,
        repo: &RepoRef,
        name: &str,
        from_ref: &str,
    ) -> ForgeResult<BranchOutcome> {
        let (in_, repo, name, from) = (
            self.inner.clone(),
            repo.clone(),
            name.to_string(),
            from_ref.to_string(),
        );
        on_rt!(async move { in_.create_branch(&repo, &name, &from).await })
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
        let (in_, repo) = (self.inner.clone(), repo.clone());
        let (branch, path, content, blob_sha, message) = (
            branch.to_string(),
            path.to_string(),
            content.to_string(),
            blob_sha.to_string(),
            message.to_string(),
        );
        on_rt!(async move {
            in_.commit_file(&repo, &branch, &path, &content, &blob_sha, &message)
                .await
        })
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
        let (in_, repo) = (self.inner.clone(), repo.clone());
        let (head, base, title, body) = (
            head.to_string(),
            base.to_string(),
            title.to_string(),
            body.to_string(),
        );
        on_rt!(async move {
            in_.open_change(&repo, &head, &base, &title, &body, remove_source_branch)
                .await
        })
    }

    async fn enable_auto_merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<bool> {
        let (in_, repo, change) = (self.inner.clone(), repo.clone(), change.clone());
        on_rt!(async move { in_.enable_auto_merge(&repo, &change).await })
    }

    async fn pipeline_status(
        &self,
        repo: &RepoRef,
        change: &ChangeRef,
    ) -> ForgeResult<PipelineStatus> {
        let (in_, repo, change) = (self.inner.clone(), repo.clone(), change.clone());
        on_rt!(async move { in_.pipeline_status(&repo, &change).await })
    }

    async fn merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<String> {
        let (in_, repo, change) = (self.inner.clone(), repo.clone(), change.clone());
        on_rt!(async move { in_.merge(&repo, &change).await })
    }

    async fn change_state(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<ChangeState> {
        let (in_, repo, change) = (self.inner.clone(), repo.clone(), change.clone());
        on_rt!(async move { in_.change_state(&repo, &change).await })
    }

    async fn list_triggers(&self, repo: &RepoRef) -> ForgeResult<Vec<Trigger>> {
        let (in_, repo) = (self.inner.clone(), repo.clone());
        on_rt!(async move { in_.list_triggers(&repo).await })
    }

    async fn ensure_trigger(
        &self,
        repo: &RepoRef,
        url: &str,
        events: &[String],
        secret: &str,
    ) -> ForgeResult<EnsuredTrigger> {
        let (in_, repo) = (self.inner.clone(), repo.clone());
        let (url, events, secret) = (url.to_string(), events.to_vec(), secret.to_string());
        on_rt!(async move { in_.ensure_trigger(&repo, &url, &events, &secret).await })
    }

    async fn capabilities(&self) -> ForgeResult<ForgeCapabilities> {
        let in_ = self.inner.clone();
        on_rt!(async move { in_.capabilities().await })
    }

    async fn set_check(
        &self,
        repo: &RepoRef,
        head_sha: &str,
        name: &str,
        status: &str,
        conclusion: &str,
        details_url: &str,
    ) -> ForgeResult<String> {
        let (in_, repo) = (self.inner.clone(), repo.clone());
        let (head_sha, name, status, conclusion, details_url) = (
            head_sha.to_string(),
            name.to_string(),
            status.to_string(),
            conclusion.to_string(),
            details_url.to_string(),
        );
        on_rt!(async move {
            in_.set_check(&repo, &head_sha, &name, &status, &conclusion, &details_url)
                .await
        })
    }

    async fn comment(&self, repo: &RepoRef, number: u64, body: &str) -> ForgeResult<String> {
        let (in_, repo, body) = (self.inner.clone(), repo.clone(), body.to_string());
        on_rt!(async move { in_.comment(&repo, number, &body).await })
    }

    #[allow(clippy::too_many_arguments)]
    async fn set_deployment(
        &self,
        repo: &RepoRef,
        head_sha: &str,
        git_ref: &str,
        environment: &str,
        state: &str,
        url: &str,
        log_url: &str,
        description: &str,
    ) -> ForgeResult<String> {
        let (in_, repo) = (self.inner.clone(), repo.clone());
        let (head_sha, git_ref, environment) = (
            head_sha.to_string(),
            git_ref.to_string(),
            environment.to_string(),
        );
        let (state, url, log_url, description) = (
            state.to_string(),
            url.to_string(),
            log_url.to_string(),
            description.to_string(),
        );
        on_rt!(async move {
            in_.set_deployment(
                &repo,
                &head_sha,
                &git_ref,
                &environment,
                &state,
                &url,
                &log_url,
                &description,
            )
            .await
        })
    }

    async fn list_repos(&self, owners: &[String], labels: &[String]) -> ForgeResult<Vec<Repository>> {
        let (in_, owners, labels) = (self.inner.clone(), owners.to_vec(), labels.to_vec());
        on_rt!(async move { in_.list_repos(&owners, &labels).await })
    }

    async fn list_issues(
        &self,
        owners: &[String],
        labels: &[String],
        for_users: &[String],
    ) -> ForgeResult<Vec<Issue>> {
        let (in_, owners, labels, for_users) = (
            self.inner.clone(),
            owners.to_vec(),
            labels.to_vec(),
            for_users.to_vec(),
        );
        on_rt!(async move { in_.list_issues(&owners, &labels, &for_users).await })
    }

    async fn list_pull_requests(
        &self,
        owners: &[String],
        labels: &[String],
        for_users: &[String],
    ) -> ForgeResult<Vec<PullRequest>> {
        let (in_, owners, labels, for_users) = (
            self.inner.clone(),
            owners.to_vec(),
            labels.to_vec(),
            for_users.to_vec(),
        );
        on_rt!(async move { in_.list_pull_requests(&owners, &labels, &for_users).await })
    }
}
