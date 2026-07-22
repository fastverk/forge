//! B8 acceptance: the `geetch` adapter passes the shared conformance suite over
//! a REAL socket.
//!
//! [`forge::geetch::GeetchForge`] is a gRPC passthrough to a daemon serving
//! `forge.v1.ForgeService`. This suite drives it against a server backed by
//! [`FakeForge`] — the same in-memory double the contract's own tests use — so
//! every case runs: conformance suite → `GeetchForge` (gRPC client) → TCP →
//! server → `FakeForge`.
//!
//! # Why FakeForge and not a real geetchd
//!
//! Because it cannot be geetchd *here*. geetch `bazel_dep`s this module; this
//! module depending on geetch to get a binary to test against would be circular.
//! So the split is: the ADAPTER and its wire-fidelity proof live here, and
//! geetch — which already deps `@forge//:forge_testing` and can build its own
//! daemon — runs the same suite against a running `geetchd` through this exact
//! adapter. Both halves are the same code path; only the server differs.
//!
//! What this file proves is the part that can actually go wrong in a
//! passthrough: that every request field survives the round trip and every
//! response is unwrapped into the right trait shape. A mapping bug shows up here
//! identically to how it would against geetchd.

use std::sync::Arc;

use forge::{
    conformance::Fixture,
    geetch::GeetchForge,
    pb::{
        forge_service_server::{ForgeService, ForgeServiceServer},
        CommitFileRequest, CommitFileResponse, CreateBranchRequest, CreateBranchResponse,
        EnableAutoMergeRequest, EnableAutoMergeResponse, EnsureTriggerRequest,
        EnsureTriggerResponse, ForgeCommentRequest, ForgeSetCheckRequest,
        ForgeSetDeploymentRequest, GetCapabilitiesRequest, GetCapabilitiesResponse,
        GetChangeStateRequest, GetChangeStateResponse, GetDefaultBranchRequest,
        GetDefaultBranchResponse, ListForgeIssuesRequest, ListForgePullRequestsRequest,
        ListForgeReposRequest, ListIssuesResponse, ListPullRequestsResponse, ListReposResponse,
        ListTriggersRequest, ListTriggersResponse, MergeRequest, MergeResponse, OpenChangeRequest,
        OpenChangeResponse, PipelineStatusRequest, PipelineStatusResponse, ReadFileRequest,
        ReadFileResponse, WriteAck,
    },
    testing::FakeForge,
    Forge, ForgeKind, RepoRef,
};
use tonic::{Request, Response, Status};

// ── a test-only ForgeService over a fixed Box<dyn Forge> ───────────────────────
//
// `gateway::ForgeGateway` cannot be reused: it picks an adapter per request from
// the RepoRef's forge kind (github/gitlab), by design — it is the multi-tenant
// front door. Here we need the opposite: one fixed backing forge, so the only
// thing under test is the wire round trip.

struct ServeForge(Arc<dyn Forge>);

/// A trait error becomes a gRPC error, the way a real server reports one — so
/// the adapter's error mapping is exercised, not bypassed.
fn to_status(e: forge::ForgeError) -> Status {
    Status::internal(e.to_string())
}

#[tonic::async_trait]
impl ForgeService for ServeForge {
    async fn get_default_branch(
        &self,
        request: Request<GetDefaultBranchRequest>,
    ) -> Result<Response<GetDefaultBranchResponse>, Status> {
        let repo = request.into_inner().repo.unwrap_or_default();
        let branch = self.0.default_branch(&repo).await.map_err(to_status)?;
        Ok(Response::new(GetDefaultBranchResponse { branch }))
    }

    async fn read_file(
        &self,
        request: Request<ReadFileRequest>,
    ) -> Result<Response<ReadFileResponse>, Status> {
        let r = request.into_inner();
        let blob = self
            .0
            .read_file(&r.repo.unwrap_or_default(), &r.path, &r.r#ref)
            .await
            .map_err(to_status)?;
        Ok(Response::new(ReadFileResponse {
            found: blob.is_some(),
            blob,
        }))
    }

    async fn create_branch(
        &self,
        request: Request<CreateBranchRequest>,
    ) -> Result<Response<CreateBranchResponse>, Status> {
        let r = request.into_inner();
        let o = self
            .0
            .create_branch(&r.repo.unwrap_or_default(), &r.name, &r.from_sha)
            .await
            .map_err(to_status)?;
        Ok(Response::new(CreateBranchResponse {
            created: o.created,
            already_existed: o.already_existed,
        }))
    }

    async fn commit_file(
        &self,
        request: Request<CommitFileRequest>,
    ) -> Result<Response<CommitFileResponse>, Status> {
        let r = request.into_inner();
        let commit_sha = self
            .0
            .commit_file(
                &r.repo.unwrap_or_default(),
                &r.branch,
                &r.path,
                &r.content,
                &r.blob_sha,
                &r.message,
            )
            .await
            .map_err(to_status)?;
        Ok(Response::new(CommitFileResponse { commit_sha }))
    }

    async fn open_change(
        &self,
        request: Request<OpenChangeRequest>,
    ) -> Result<Response<OpenChangeResponse>, Status> {
        let r = request.into_inner();
        let o = self
            .0
            .open_change(
                &r.repo.unwrap_or_default(),
                &r.head,
                &r.base,
                &r.title,
                &r.body,
                r.remove_source_branch,
            )
            .await
            .map_err(to_status)?;
        Ok(Response::new(OpenChangeResponse {
            change: Some(o.change),
            already_existed: o.already_existed,
        }))
    }

    async fn enable_auto_merge(
        &self,
        request: Request<EnableAutoMergeRequest>,
    ) -> Result<Response<EnableAutoMergeResponse>, Status> {
        let r = request.into_inner();
        let enabled = self
            .0
            .enable_auto_merge(&r.repo.unwrap_or_default(), &r.change.unwrap_or_default())
            .await
            .map_err(to_status)?;
        Ok(Response::new(EnableAutoMergeResponse { enabled }))
    }

    async fn pipeline_status(
        &self,
        request: Request<PipelineStatusRequest>,
    ) -> Result<Response<PipelineStatusResponse>, Status> {
        let r = request.into_inner();
        let s = self
            .0
            .pipeline_status(&r.repo.unwrap_or_default(), &r.change.unwrap_or_default())
            .await
            .map_err(to_status)?;
        Ok(Response::new(PipelineStatusResponse {
            status: s.status as i32,
            pipeline_id: s.pipeline_id,
            pipeline_url: s.url,
        }))
    }

    async fn merge(
        &self,
        request: Request<MergeRequest>,
    ) -> Result<Response<MergeResponse>, Status> {
        let r = request.into_inner();
        let merge_commit_sha = self
            .0
            .merge(&r.repo.unwrap_or_default(), &r.change.unwrap_or_default())
            .await
            .map_err(to_status)?;
        Ok(Response::new(MergeResponse { merge_commit_sha }))
    }

    async fn get_change_state(
        &self,
        request: Request<GetChangeStateRequest>,
    ) -> Result<Response<GetChangeStateResponse>, Status> {
        let r = request.into_inner();
        let state = self
            .0
            .change_state(&r.repo.unwrap_or_default(), &r.change.unwrap_or_default())
            .await
            .map_err(to_status)?;
        Ok(Response::new(GetChangeStateResponse {
            state: state as i32,
            merge_commit_sha: String::new(),
        }))
    }

    async fn list_triggers(
        &self,
        request: Request<ListTriggersRequest>,
    ) -> Result<Response<ListTriggersResponse>, Status> {
        let r = request.into_inner();
        let triggers = self
            .0
            .list_triggers(&r.repo.unwrap_or_default())
            .await
            .map_err(to_status)?;
        Ok(Response::new(ListTriggersResponse { triggers }))
    }

    async fn ensure_trigger(
        &self,
        request: Request<EnsureTriggerRequest>,
    ) -> Result<Response<EnsureTriggerResponse>, Status> {
        let r = request.into_inner();
        let e = self
            .0
            .ensure_trigger(&r.repo.unwrap_or_default(), &r.url, &r.events, &r.secret)
            .await
            .map_err(to_status)?;
        Ok(Response::new(EnsureTriggerResponse {
            trigger: Some(e.trigger),
            created: e.created,
        }))
    }

    // The optional surfaces folded into ForgeService. Delegated the same way as
    // everything above, so a passthrough field-mapping bug in any of them shows
    // up here. `FakeForge` takes the trait's `unsupported` defaults for all but
    // capabilities, which is itself the case worth serving: it is what lets a
    // caller learn the rest are absent WITHOUT calling them.
    async fn get_capabilities(
        &self,
        request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<GetCapabilitiesResponse>, Status> {
        let _ = request.into_inner();
        let capabilities = self.0.capabilities().await.map_err(to_status)?;
        Ok(Response::new(GetCapabilitiesResponse {
            capabilities: Some(capabilities),
        }))
    }

    async fn set_check(
        &self,
        request: Request<ForgeSetCheckRequest>,
    ) -> Result<Response<WriteAck>, Status> {
        let r = request.into_inner();
        let detail = self
            .0
            .set_check(
                &r.repo.unwrap_or_default(),
                &r.head_sha,
                &r.name,
                &r.status,
                &r.conclusion,
                &r.details_url,
            )
            .await
            .map_err(to_status)?;
        Ok(Response::new(WriteAck { ok: true, detail }))
    }

    async fn comment(
        &self,
        request: Request<ForgeCommentRequest>,
    ) -> Result<Response<WriteAck>, Status> {
        let r = request.into_inner();
        let detail = self
            .0
            .comment(&r.repo.unwrap_or_default(), r.number, &r.body)
            .await
            .map_err(to_status)?;
        Ok(Response::new(WriteAck { ok: true, detail }))
    }

    async fn set_deployment(
        &self,
        request: Request<ForgeSetDeploymentRequest>,
    ) -> Result<Response<WriteAck>, Status> {
        let r = request.into_inner();
        let detail = self
            .0
            .set_deployment(
                &r.repo.unwrap_or_default(),
                &r.head_sha,
                &r.r#ref,
                &r.environment,
                &r.state,
                &r.url,
                &r.log_url,
                &r.description,
            )
            .await
            .map_err(to_status)?;
        Ok(Response::new(WriteAck { ok: true, detail }))
    }

    async fn list_repos(
        &self,
        request: Request<ListForgeReposRequest>,
    ) -> Result<Response<ListReposResponse>, Status> {
        let r = request.into_inner();
        let repos = self
            .0
            .list_repos(&r.owners, &r.labels)
            .await
            .map_err(to_status)?;
        Ok(Response::new(ListReposResponse { repos }))
    }

    async fn list_issues(
        &self,
        request: Request<ListForgeIssuesRequest>,
    ) -> Result<Response<ListIssuesResponse>, Status> {
        let r = request.into_inner();
        let issues = self
            .0
            .list_issues(&r.owners, &r.labels, &r.for_users)
            .await
            .map_err(to_status)?;
        Ok(Response::new(ListIssuesResponse { issues }))
    }

    async fn list_pull_requests(
        &self,
        request: Request<ListForgePullRequestsRequest>,
    ) -> Result<Response<ListPullRequestsResponse>, Status> {
        let r = request.into_inner();
        let prs = self
            .0
            .list_pull_requests(&r.owners, &r.labels, &r.for_users)
            .await
            .map_err(to_status)?;
        Ok(Response::new(ListPullRequestsResponse { prs }))
    }
}

// ── the fixture ────────────────────────────────────────────────────────────────

/// A `FakeForge` served over a real TCP socket, with the B8 adapter pointed at
/// it. Each conformance case builds a fresh one (its own port and its own
/// backing state), so cases cannot leak into one another.
struct GrpcFixture {
    adapter: GeetchForge,
    repo: RepoRef,
    /// Kept so the suite can inject a one-shot failure: the fake is in THIS
    /// process, even though every call reaches it over the wire.
    fake: Arc<FakeForge>,
}

impl GrpcFixture {
    async fn new() -> Self {
        let fake = Arc::new(FakeForge::new(ForgeKind::Geetch));
        fake.seed_repo("acme/widgets", "main");

        // Bind first, then serve from the bound listener, so there is no
        // bind→drop→rebind window for a parallel test to race into.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let svc = ServeForge(Arc::clone(&fake) as Arc<dyn Forge>);
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(ForgeServiceServer::new(svc))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .expect("server");
        });

        let adapter = GeetchForge::connect(&format!("http://{addr}"))
            .await
            .expect("the adapter must reach the server");

        Self {
            adapter,
            repo: RepoRef {
                forge: ForgeKind::Geetch as i32,
                host: String::new(),
                owner: "acme".into(),
                name: "widgets".into(),
            },
            fake,
        }
    }
}

impl Fixture for GrpcFixture {
    fn forge(&self) -> &dyn Forge {
        &self.adapter
    }

    fn repo(&self) -> RepoRef {
        self.repo.clone()
    }

    /// Supported here, unlike a real-forge fixture: the fake is in-process, so a
    /// blip can be synthesized even though the call travels over gRPC. That makes
    /// the network path cover error propagation too — a `Status` coming back must
    /// surface as a `ForgeError`, and the retry must still create the branch.
    fn inject_failure(&self, method: &str, msg: &str) -> bool {
        self.fake.fail_next(method, msg);
        true
    }
}

// The adapter reports `kind() == Geetch`, so this really is the geetch path.
forge::conformance_suite!(geetch_over_grpc, GrpcFixture::new().await);
