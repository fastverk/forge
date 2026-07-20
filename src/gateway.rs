//! forge-gateway — the gRPC daemon serving `forge.v1.ForgeService`.
//!
//! The single-source-of-truth server for forge operations: it implements the
//! generated `ForgeService` over the crate's [`Forge`] trait, dispatching each
//! RPC to a per-request [`GitLabForge`]/[`GitHubForge`] adapter. The daemon holds
//! **no** forge credential of its own — the caller's identity travels in request
//! metadata (`x-fastverk-gitlab-token` / `-host`, `x-fastverk-github-token`),
//! exactly like the plugin HTTP facade forwards `X-Fastverk-*` headers — so every
//! op runs as the caller.
//!
//! Both consumers share this one implementation: the `wave` cascade engine (which
//! today uses the [`Forge`] trait in-process) can dial it over gRPC, and the
//! console's agent-callable MCP write tools proxy to it, so GitLab MR
//! create/auto-merge/merge live in one place.

use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

use crate::github::GitHubForge;
use crate::gitlab::GitLabForge;
use crate::pb::forge_service_server::{ForgeService, ForgeServiceServer};
use crate::pb::{
    CommitFileRequest, CommitFileResponse, CreateBranchRequest, CreateBranchResponse,
    EnableAutoMergeRequest, EnableAutoMergeResponse, EnsureTriggerRequest, EnsureTriggerResponse,
    GetChangeStateRequest, GetChangeStateResponse, GetDefaultBranchRequest,
    GetDefaultBranchResponse, ListTriggersRequest, ListTriggersResponse, MergeRequest,
    MergeResponse, OpenChangeRequest, OpenChangeResponse, PipelineStatusRequest,
    PipelineStatusResponse, ReadFileRequest, ReadFileResponse,
};
use crate::{Forge, ForgeError, ForgeKind, RepoRef};

/// Metadata key carrying the caller's self-hosted GitLab token.
const GITLAB_TOKEN_META: &str = "x-fastverk-gitlab-token";
/// Metadata key carrying the caller's GitLab instance host (self-hosted, so the
/// host travels with the token). Falls back to `RepoRef.host` when absent.
const GITLAB_HOST_META: &str = "x-fastverk-gitlab-host";
/// Metadata key carrying the caller's GitHub token.
const GITHUB_TOKEN_META: &str = "x-fastverk-github-token";

/// A seam for building the per-request forge adapter from a repo ref and the
/// caller's metadata credentials. Implement this trait to inject a test double
/// (e.g. `forge::testing::FakeForge`) without touching the live adapter code.
///
/// The default production implementation is [`DefaultForgeFactory`].
pub trait ForgeFactory: Send + Sync {
    /// Construct a [`Forge`] adapter for `repo`, extracting whatever credentials
    /// are needed from `meta`. Returns a gRPC [`Status`] error when a required
    /// credential is absent or the forge kind is unsupported.
    ///
    // `tonic::Status` is an opaque external type that cannot be made smaller;
    // boxing it would require every call site to `map_err(|e| *e)` after each
    // `?` with no practical benefit — the stack frame lives only for the
    // duration of a single RPC dispatch.
    #[allow(clippy::result_large_err)]
    fn build(&self, repo: &RepoRef, meta: &MetadataMap) -> Result<Box<dyn Forge>, Status>;
}

/// The production [`ForgeFactory`]: builds a [`GitLabForge`] or [`GitHubForge`]
/// from the caller's request metadata, exactly as `ForgeGateway` used to do
/// inline before the seam was extracted.
pub struct DefaultForgeFactory;

impl ForgeFactory for DefaultForgeFactory {
    #[allow(clippy::result_large_err)]
    fn build(&self, repo: &RepoRef, meta: &MetadataMap) -> Result<Box<dyn Forge>, Status> {
        match ForgeKind::try_from(repo.forge).unwrap_or(ForgeKind::Unspecified) {
            ForgeKind::Gitlab => {
                let token = meta_str(meta, GITLAB_TOKEN_META)
                    .ok_or_else(|| Status::unauthenticated("missing gitlab token"))?;
                let host = if repo.host.is_empty() {
                    meta_str(meta, GITLAB_HOST_META).unwrap_or_default()
                } else {
                    repo.host.clone()
                };
                if host.is_empty() {
                    return Err(Status::invalid_argument(
                        "gitlab host required (repo.host or metadata)",
                    ));
                }
                Ok(Box::new(GitLabForge::new(host, token).map_err(to_status)?))
            }
            ForgeKind::Github => {
                let token = meta_str(meta, GITHUB_TOKEN_META)
                    .ok_or_else(|| Status::unauthenticated("missing github token"))?;
                Ok(Box::new(GitHubForge::new(token).map_err(to_status)?))
            }
            ForgeKind::Unspecified => Err(Status::invalid_argument(
                "repo.forge must be FORGE_GITLAB or FORGE_GITHUB",
            )),
        }
    }
}

/// The `forge.v1.ForgeService` implementation.
pub struct ForgeGateway {
    factory: Box<dyn ForgeFactory>,
}

impl Default for ForgeGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl ForgeGateway {
    /// Create a gateway backed by the production [`DefaultForgeFactory`].
    pub fn new() -> Self {
        Self {
            factory: Box::new(DefaultForgeFactory),
        }
    }

    /// Create a gateway backed by a custom factory (e.g. a test double).
    pub fn with_factory(factory: Box<dyn ForgeFactory>) -> Self {
        Self { factory }
    }

    /// Wrap the gateway in its tonic server, ready to `add_service`.
    pub fn into_server(self) -> ForgeServiceServer<Self> {
        ForgeServiceServer::new(self)
    }

    /// Build the per-request forge adapter for `repo` from the caller's metadata
    /// credentials. Delegates to the injected [`ForgeFactory`].
    #[allow(clippy::result_large_err)]
    fn adapter(&self, repo: &RepoRef, meta: &MetadataMap) -> Result<Box<dyn Forge>, Status> {
        self.factory.build(repo, meta)
    }
}

/// Split a request into its metadata + the required `repo`, and build the adapter.
/// The common preamble for every RPC below.
macro_rules! adapter_for {
    ($self:ident, $req:ident) => {{
        let (meta, _ext, msg) = $req.into_parts();
        let repo = msg
            .repo
            .clone()
            .ok_or_else(|| Status::invalid_argument("repo is required"))?;
        let forge = $self.adapter(&repo, &meta)?;
        (forge, repo, msg)
    }};
}

#[tonic::async_trait]
impl ForgeService for ForgeGateway {
    async fn get_default_branch(
        &self,
        req: Request<GetDefaultBranchRequest>,
    ) -> Result<Response<GetDefaultBranchResponse>, Status> {
        let (forge, repo, _msg) = adapter_for!(self, req);
        let branch = forge.default_branch(&repo).await.map_err(to_status)?;
        Ok(Response::new(GetDefaultBranchResponse { branch }))
    }

    async fn read_file(
        &self,
        req: Request<ReadFileRequest>,
    ) -> Result<Response<ReadFileResponse>, Status> {
        let (forge, repo, msg) = adapter_for!(self, req);
        let blob = forge
            .read_file(&repo, &msg.path, &msg.r#ref)
            .await
            .map_err(to_status)?;
        Ok(Response::new(ReadFileResponse {
            found: blob.is_some(),
            blob,
        }))
    }

    async fn create_branch(
        &self,
        req: Request<CreateBranchRequest>,
    ) -> Result<Response<CreateBranchResponse>, Status> {
        let (forge, repo, msg) = adapter_for!(self, req);
        let out = forge
            .create_branch(&repo, &msg.name, &msg.from_sha)
            .await
            .map_err(to_status)?;
        Ok(Response::new(CreateBranchResponse {
            created: out.created,
            already_existed: out.already_existed,
        }))
    }

    async fn commit_file(
        &self,
        req: Request<CommitFileRequest>,
    ) -> Result<Response<CommitFileResponse>, Status> {
        let (forge, repo, msg) = adapter_for!(self, req);
        let commit_sha = forge
            .commit_file(
                &repo,
                &msg.branch,
                &msg.path,
                &msg.content,
                &msg.blob_sha,
                &msg.message,
            )
            .await
            .map_err(to_status)?;
        Ok(Response::new(CommitFileResponse { commit_sha }))
    }

    async fn open_change(
        &self,
        req: Request<OpenChangeRequest>,
    ) -> Result<Response<OpenChangeResponse>, Status> {
        let (forge, repo, msg) = adapter_for!(self, req);
        let out = forge
            .open_change(
                &repo,
                &msg.head,
                &msg.base,
                &msg.title,
                &msg.body,
                msg.remove_source_branch,
            )
            .await
            .map_err(to_status)?;
        Ok(Response::new(OpenChangeResponse {
            change: Some(out.change),
            already_existed: out.already_existed,
        }))
    }

    async fn enable_auto_merge(
        &self,
        req: Request<EnableAutoMergeRequest>,
    ) -> Result<Response<EnableAutoMergeResponse>, Status> {
        let (forge, repo, msg) = adapter_for!(self, req);
        let change = msg
            .change
            .ok_or_else(|| Status::invalid_argument("change is required"))?;
        let enabled = forge
            .enable_auto_merge(&repo, &change)
            .await
            .map_err(to_status)?;
        Ok(Response::new(EnableAutoMergeResponse { enabled }))
    }

    async fn pipeline_status(
        &self,
        req: Request<PipelineStatusRequest>,
    ) -> Result<Response<PipelineStatusResponse>, Status> {
        let (forge, repo, msg) = adapter_for!(self, req);
        let change = msg
            .change
            .ok_or_else(|| Status::invalid_argument("change is required"))?;
        let ps = forge
            .pipeline_status(&repo, &change)
            .await
            .map_err(to_status)?;
        Ok(Response::new(PipelineStatusResponse {
            status: ps.status as i32,
            pipeline_id: ps.pipeline_id,
            pipeline_url: ps.url,
        }))
    }

    async fn merge(&self, req: Request<MergeRequest>) -> Result<Response<MergeResponse>, Status> {
        let (forge, repo, msg) = adapter_for!(self, req);
        let change = msg
            .change
            .ok_or_else(|| Status::invalid_argument("change is required"))?;
        let merge_commit_sha = forge.merge(&repo, &change).await.map_err(to_status)?;
        Ok(Response::new(MergeResponse { merge_commit_sha }))
    }

    async fn get_change_state(
        &self,
        req: Request<GetChangeStateRequest>,
    ) -> Result<Response<GetChangeStateResponse>, Status> {
        let (forge, repo, msg) = adapter_for!(self, req);
        let change = msg
            .change
            .ok_or_else(|| Status::invalid_argument("change is required"))?;
        let state = forge
            .change_state(&repo, &change)
            .await
            .map_err(to_status)?;
        Ok(Response::new(GetChangeStateResponse {
            state: state as i32,
            // The trait reports state only; the merge sha is available via Merge.
            merge_commit_sha: String::new(),
        }))
    }

    async fn list_triggers(
        &self,
        req: Request<ListTriggersRequest>,
    ) -> Result<Response<ListTriggersResponse>, Status> {
        let (forge, repo, _msg) = adapter_for!(self, req);
        let triggers = forge.list_triggers(&repo).await.map_err(to_status)?;
        Ok(Response::new(ListTriggersResponse { triggers }))
    }

    async fn ensure_trigger(
        &self,
        req: Request<EnsureTriggerRequest>,
    ) -> Result<Response<EnsureTriggerResponse>, Status> {
        let (forge, repo, msg) = adapter_for!(self, req);
        let out = forge
            .ensure_trigger(&repo, &msg.url, &msg.events, &msg.secret)
            .await
            .map_err(to_status)?;
        Ok(Response::new(EnsureTriggerResponse {
            trigger: Some(out.trigger),
            created: out.created,
        }))
    }
}

/// Read a metadata value as a `String` (ASCII), if present and non-empty.
fn meta_str(meta: &MetadataMap, key: &str) -> Option<String> {
    meta.get(key)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// A forge-op error becomes an internal gRPC status (the message is already the
/// adapter's human-readable cause).
fn to_status(e: ForgeError) -> Status {
    Status::internal(e.to_string())
}
