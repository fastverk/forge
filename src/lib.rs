//! Generic forge contract + GitHub/GitLab adapters.
//!
//! [`Forge`] is the set of operations a cross-repo cascade needs from a code
//! host: read the default branch + a file, create a branch, commit, open a
//! change (PR/MR), enable auto-merge, poll the pipeline, merge, and read the
//! change's state. The DTOs ([`RepoRef`], [`ChangeRef`], [`FileBlob`],
//! [`CiStatus`], [`ChangeState`]) are proto messages (package `forge.v1`); the
//! generated gRPC `ForgeService` is the same contract for a future
//! forge-gateway daemon. In-process consumers (the wave engine) use the async
//! [`Forge`] trait directly.

use async_trait::async_trait;

pub mod pb {
    //! Generated `forge.v1` proto types + gRPC service stubs.
    tonic::include_proto!("forge.v1");
}

pub mod gateway;
// geetch — the platform's own forge. Unlike github/gitlab this adapter maps
// nothing: geetch SERVES forge.v1 natively, so it is a gRPC passthrough.
pub mod geetch;
pub mod github;
pub mod gitlab;
pub mod provision;
// Runtime pinning for out-of-module consumers — see the module docs for the
// cross-crate_universe reactor panic it exists to prevent.
pub mod runtime;

// Test doubles for the `Forge` contract. A real module behind a non-default
// feature, NOT `#[cfg(test)]`: wave / plugin-forge / geetch are each a separate
// Bazel module and cannot see this crate's test-only code. Same reasoning that
// made `ForgeError` concrete rather than `anyhow`.
#[cfg(feature = "testing")]
pub mod testing;

// The conformance suite — the executable specification every adapter is held to.
// Also behind `testing`, and for the same reason one level up: it began in
// `tests/conformance.rs`, and Rust integration tests are not importable by other
// crates, so an out-of-crate adapter could not run the shared battery. geetch's
// B2 hit that wall and copied all eight cases into its own repo — which proves
// nothing about the shared contract, because a copy drifts.
#[cfg(feature = "testing")]
pub mod conformance;

pub use pb::{ChangeRef, ChangeState, CiStatus, FileBlob, Forge as ForgeKind, RepoRef, Trigger};
// Optional-surface declaration + the discovery DTOs the folded-in read RPCs
// return. `Issue`/`PullRequest`/`Repository` were defined in discovery.proto for
// the cross-forge fan-out; the per-forge legs answer with the same types.
pub use pb::{ForgeCapabilities, Issue, PullRequest, Repository};

/// A forge operation error. A concrete type (not `anyhow`) so the public `Forge`
/// API doesn't leak `anyhow` — which also lets consumers in a *different* crate
/// universe (a separate Bazel module) implement + call the trait without the
/// two `anyhow` instances colliding. Adapters build it from their internal
/// `anyhow` errors via `From`; consumers turn it back into their own error type
/// (it's a `std::error::Error`, so `anyhow`'s blanket `?` just works).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ForgeError(String);

impl ForgeError {
    /// A `ForgeError` from any displayable message.
    pub fn msg(m: impl std::fmt::Display) -> Self {
        Self(m.to_string())
    }
}

impl From<anyhow::Error> for ForgeError {
    fn from(e: anyhow::Error) -> Self {
        Self(format!("{e:#}"))
    }
}

/// `Result` for [`Forge`] operations.
pub type ForgeResult<T> = Result<T, ForgeError>;

/// Outcome of [`Forge::create_branch`] — idempotent over an existing branch.
#[derive(Debug, Clone)]
pub struct BranchOutcome {
    pub created: bool,
    pub already_existed: bool,
}

/// Outcome of [`Forge::open_change`] — idempotent over an existing open change.
#[derive(Debug, Clone)]
pub struct OpenedChange {
    pub change: ChangeRef,
    pub already_existed: bool,
}

/// A change's pipeline status + identity.
#[derive(Debug, Clone)]
pub struct PipelineStatus {
    pub status: CiStatus,
    pub pipeline_id: String,
    pub url: String,
}

/// Outcome of [`Forge::ensure_trigger`] — idempotent over an existing hook.
#[derive(Debug, Clone)]
pub struct EnsuredTrigger {
    pub trigger: Trigger,
    /// True if this call created the hook; false if it already existed.
    pub created: bool,
}

/// The default inbound events a build trigger subscribes to, in the normalized
/// (GitHub) vocabulary. The GitLab adapter maps these to its boolean flags.
#[must_use]
pub fn default_trigger_events() -> Vec<String> {
    vec!["push".to_string(), "pull_request".to_string()]
}

/// Convenience: `owner/name` for a repo (owner may be a nested group path).
#[must_use]
pub fn repo_slug(repo: &RepoRef) -> String {
    if repo.owner.is_empty() {
        repo.name.clone()
    } else {
        format!("{}/{}", repo.owner, repo.name)
    }
}

/// The operations a forge (GitHub, GitLab, …) provides to a cascade. All
/// methods are idempotent where noted, so a resuming reconcile loop can call
/// them repeatedly without creating duplicates.
#[async_trait]
pub trait Forge: Send + Sync {
    /// Which forge this adapter targets.
    fn kind(&self) -> ForgeKind;

    /// The repo's default branch (e.g. "main").
    async fn default_branch(&self, repo: &RepoRef) -> ForgeResult<String>;

    /// Read a file at `r#ref` (empty = default branch). `Ok(None)` = absent.
    async fn read_file(
        &self,
        repo: &RepoRef,
        path: &str,
        r#ref: &str,
    ) -> ForgeResult<Option<FileBlob>>;

    /// Create branch `name` from `from_ref` (a branch name or sha). Idempotent:
    /// an existing branch returns `already_existed = true`, not an error.
    async fn create_branch(
        &self,
        repo: &RepoRef,
        name: &str,
        from_ref: &str,
    ) -> ForgeResult<BranchOutcome>;

    /// Commit `content` to `path` on `branch`. `blob_sha` is the opaque id from
    /// [`Forge::read_file`] (GitHub blob sha / GitLab last_commit_id), required
    /// to update an existing file. Returns the commit sha when the forge
    /// reports it (may be empty otherwise).
    async fn commit_file(
        &self,
        repo: &RepoRef,
        branch: &str,
        path: &str,
        content: &str,
        blob_sha: &str,
        message: &str,
    ) -> ForgeResult<String>;

    /// Open a PR/MR from `head` into `base`. Idempotent: an existing open
    /// change for `head` is returned with `already_existed = true`.
    async fn open_change(
        &self,
        repo: &RepoRef,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        remove_source_branch: bool,
    ) -> ForgeResult<OpenedChange>;

    /// Enable auto-merge / merge-when-pipeline-succeeds. Returns whether it was
    /// enabled (false if the forge merged immediately or it's unavailable).
    async fn enable_auto_merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<bool>;

    /// The pipeline status for the change's head.
    async fn pipeline_status(
        &self,
        repo: &RepoRef,
        change: &ChangeRef,
    ) -> ForgeResult<PipelineStatus>;

    /// Merge the change now. Returns the merge commit sha.
    async fn merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<String>;

    /// The change's open/merged/closed state.
    async fn change_state(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<ChangeState>;

    /// List the repo's configured inbound triggers (webhooks) — how a caller
    /// reads "does this repo have incoming triggers established?" straight from
    /// the forge (feeds a readiness criterion). Default: unsupported, so an
    /// out-of-crate adapter that predates this method still compiles.
    async fn list_triggers(&self, _repo: &RepoRef) -> ForgeResult<Vec<Trigger>> {
        Err(ForgeError::msg(
            "list_triggers not supported by this forge adapter",
        ))
    }

    /// Idempotently ensure an inbound trigger at `url` subscribed to `events`
    /// (empty ⇒ [`default_trigger_events`]) with `secret` as its verification
    /// secret (empty ⇒ leave unset). An existing hook at `url` returns
    /// `created = false`. Default: unsupported (see [`Forge::list_triggers`]).
    async fn ensure_trigger(
        &self,
        _repo: &RepoRef,
        _url: &str,
        _events: &[String],
        _secret: &str,
    ) -> ForgeResult<EnsuredTrigger> {
        Err(ForgeError::msg(
            "ensure_trigger not supported by this forge adapter",
        ))
    }

    /// Which optional surfaces this adapter WILL SERVE — ask before attempting
    /// one, instead of calling and interpreting the error.
    ///
    /// Scope is deliberately the ADAPTER, not the forge product. "GitHub has an
    /// issue tracker" is true and useless here; what a caller needs to know is
    /// whether *this* object will answer `list_issues`. Only the adapter-scoped
    /// reading lets a caller skip a surface safely, which is the whole point.
    /// So an adapter that targets a forge with issues but has not wired
    /// `list_issues` reports `issues = false` — and flips it on in the same
    /// commit that implements the method.
    ///
    /// The default declares NOTHING optional, which is the conservative
    /// direction: an adapter that predates a capability is assumed not to have
    /// it, so a caller skips a surface that might have worked. The opposite
    /// default has callers attempt surfaces that cannot exist — the failure this
    /// exists to remove.
    ///
    /// `Err` means "could not ask" (an unreachable remote), which is NOT the
    /// same as "has nothing" — a passthrough adapter must not silently downgrade
    /// a dead connection into an all-false answer.
    ///
    /// This describes the adapter, not one repository. A per-repo toggle (GitHub
    /// lets an individual repo turn issues off) is a different question and
    /// belongs on the repository.
    async fn capabilities(&self) -> ForgeResult<ForgeCapabilities> {
        Ok(ForgeCapabilities::default())
    }

    // ── write-back ────────────────────────────────────────────────────────────
    //
    // Reporting build/deploy state back to the forge. Per-repo, and defaulted
    // unsupported for the same reason as the trigger methods above: an
    // out-of-crate adapter written against an earlier contract still compiles.

    /// Post a commit check / status. Returns a forge-specific detail string
    /// (an id or URL) purely for logging.
    async fn set_check(
        &self,
        _repo: &RepoRef,
        _head_sha: &str,
        _name: &str,
        _status: &str,
        _conclusion: &str,
        _details_url: &str,
    ) -> ForgeResult<String> {
        Err(ForgeError::msg(
            "set_check not supported by this forge adapter",
        ))
    }

    /// Comment on change or issue `number` in `repo`.
    async fn comment(&self, _repo: &RepoRef, _number: u64, _body: &str) -> ForgeResult<String> {
        Err(ForgeError::msg(
            "comment not supported by this forge adapter",
        ))
    }

    /// Record a deployment / environment status — the "Environment · Ready ·
    /// Visit" surface. `git_ref` is the deployed ref ("refs/heads/main"); `state`
    /// is the forge-agnostic vocabulary
    /// (queued|in_progress|success|failure|inactive), which the adapter maps.
    #[allow(clippy::too_many_arguments)]
    async fn set_deployment(
        &self,
        _repo: &RepoRef,
        _head_sha: &str,
        _git_ref: &str,
        _environment: &str,
        _state: &str,
        _url: &str,
        _log_url: &str,
        _description: &str,
    ) -> ForgeResult<String> {
        Err(ForgeError::msg(
            "set_deployment not supported by this forge adapter",
        ))
    }

    // ── discovery, for THIS forge only ────────────────────────────────────────
    //
    // Each answers for the forge this adapter targets. Aggregating across several
    // forges is a layer ABOVE the trait: it fans out over adapters and
    // concatenates, which is why these take no `forges` selector. Gate them on
    // `capabilities()` rather than calling and interpreting the error.

    /// Repositories visible to this adapter under `owners` (empty = its default).
    async fn list_repos(
        &self,
        _owners: &[String],
        _labels: &[String],
    ) -> ForgeResult<Vec<Repository>> {
        Err(ForgeError::msg(
            "list_repos not supported by this forge adapter",
        ))
    }

    /// Open issues. A forge with no issue tracker reports `issues = false` from
    /// [`Forge::capabilities`] and is simply not asked.
    async fn list_issues(
        &self,
        _owners: &[String],
        _labels: &[String],
        _for_users: &[String],
    ) -> ForgeResult<Vec<Issue>> {
        Err(ForgeError::msg(
            "list_issues not supported by this forge adapter",
        ))
    }

    /// Open pull/merge requests (changes).
    async fn list_pull_requests(
        &self,
        _owners: &[String],
        _labels: &[String],
        _for_users: &[String],
    ) -> ForgeResult<Vec<PullRequest>> {
        Err(ForgeError::msg(
            "list_pull_requests not supported by this forge adapter",
        ))
    }
}
