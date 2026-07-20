//! Offline tests for the [`ForgeGateway`] gRPC dispatch layer.
//!
//! Every test here runs with no network and no credentials by injecting a
//! [`FakeForgeFactory`] that bypasses auth and returns an in-memory
//! [`FakeForge`]. The only tests that use the real [`DefaultForgeFactory`]
//! (i.e. the production path) are the auth/validation cases that fail *before*
//! any forge adapter is constructed — so they need no network either.
//!
//! # Cases covered
//!
//! - missing `x-fastverk-gitlab-token` → `unauthenticated`
//! - `FORGE_UNSPECIFIED` → `invalid_argument`
//! - missing `repo` → `invalid_argument`
//! - missing `change` on Merge / PipelineStatus / EnableAutoMerge /
//!   GetChangeState → `invalid_argument`
//! - `ReadFile` on an absent path → `found: false, blob: None`
//! - `GetChangeState` returns an empty `merge_commit_sha` (documented quirk)
//! - a [`ForgeError`] is surfaced as `Status::internal` with the message intact

use std::sync::Arc;

use forge::gateway::ForgeGateway;
use forge::pb::forge_service_server::ForgeService;
use forge::pb::{
    EnableAutoMergeRequest, GetChangeStateRequest, GetDefaultBranchRequest, MergeRequest,
    PipelineStatusRequest, ReadFileRequest,
};
use forge::testing::{FakeForge, FakeForgeFactory};
use forge::{ChangeRef, ForgeKind, RepoRef};
use tonic::metadata::MetadataMap;
use tonic::{Code, Request};

// ── helpers ──────────────────────────────────────────────────────────────────

/// A [`RepoRef`] that identifies a GitLab project already seeded in the fake.
fn gitlab_repo() -> RepoRef {
    RepoRef {
        forge: ForgeKind::Gitlab as i32,
        host: "gitlab.example.com".to_string(),
        owner: "acme".to_string(),
        name: "widgets".to_string(),
    }
}

/// A [`RepoRef`] with `forge = FORGE_UNSPECIFIED` (value 0).
fn unspecified_repo() -> RepoRef {
    RepoRef {
        forge: 0, // FORGE_UNSPECIFIED
        host: String::new(),
        owner: "acme".to_string(),
        name: "widgets".to_string(),
    }
}

/// A [`ChangeRef`] matching the change seeded by [`gateway_with_change`].
fn seeded_change() -> ChangeRef {
    ChangeRef {
        number: 1,
        url: String::new(),
        branch: "feat/x".to_string(),
    }
}

/// Build a gateway backed by `fake` (shared via `Arc`) and pre-seeded with one
/// repo and one change.
fn gateway_with_change(fake: &Arc<FakeForge>) -> ForgeGateway {
    fake.seed_repo("acme/widgets", "main");
    fake.seed_change("acme/widgets", 1, "feat/x", "main");
    ForgeGateway::with_factory(Box::new(FakeForgeFactory(Arc::clone(fake))))
}

/// Build a gateway backed by `fake` and pre-seeded with one repo (no change).
fn gateway_with_repo(fake: &Arc<FakeForge>) -> ForgeGateway {
    fake.seed_repo("acme/widgets", "main");
    ForgeGateway::with_factory(Box::new(FakeForgeFactory(Arc::clone(fake))))
}

/// Attach a GitLab token header to any request.  The real `DefaultForgeFactory`
/// requires this; `FakeForgeFactory` ignores it.  It is included in the
/// auth-positive tests so the failure is in the right layer.
fn with_gitlab_token<T>(req: T) -> Request<T> {
    let mut r = Request::new(req);
    r.metadata_mut()
        .insert("x-fastverk-gitlab-token", "tok".parse().unwrap());
    r
}

// ── 1. missing gitlab token → unauthenticated ────────────────────────────────

#[tokio::test]
async fn missing_gitlab_token_is_unauthenticated() {
    // Uses the real DefaultForgeFactory; no network is touched because the
    // error fires before any HTTP client is created.
    let gw = ForgeGateway::new();

    // No token header — request carries only the repo ref.
    let mut meta = MetadataMap::new();
    meta.insert(
        "x-fastverk-gitlab-host",
        "gitlab.example.com".parse().unwrap(),
    );

    let req = Request::from_parts(
        meta,
        Default::default(),
        GetDefaultBranchRequest {
            repo: Some(gitlab_repo()),
        },
    );

    let err = gw.get_default_branch(req).await.unwrap_err();
    assert_eq!(
        err.code(),
        Code::Unauthenticated,
        "expected Unauthenticated, got: {err:?}"
    );
}

// ── 2. FORGE_UNSPECIFIED → invalid_argument ───────────────────────────────────

#[tokio::test]
async fn forge_unspecified_is_invalid_argument() {
    let gw = ForgeGateway::new();

    let req = Request::new(GetDefaultBranchRequest {
        repo: Some(unspecified_repo()),
    });

    let err = gw.get_default_branch(req).await.unwrap_err();
    assert_eq!(
        err.code(),
        Code::InvalidArgument,
        "expected InvalidArgument for FORGE_UNSPECIFIED, got: {err:?}"
    );
}

// ── 3. missing repo → invalid_argument ───────────────────────────────────────

#[tokio::test]
async fn missing_repo_is_invalid_argument() {
    let gw = ForgeGateway::new();

    let req = Request::new(GetDefaultBranchRequest { repo: None });

    let err = gw.get_default_branch(req).await.unwrap_err();
    assert_eq!(
        err.code(),
        Code::InvalidArgument,
        "expected InvalidArgument for missing repo, got: {err:?}"
    );
}

// ── 4. missing `change` on each relevant RPC → invalid_argument ──────────────

#[tokio::test]
async fn merge_missing_change_is_invalid_argument() {
    let fake = Arc::new(FakeForge::new(ForgeKind::Gitlab));
    let gw = gateway_with_repo(&fake);

    let req = with_gitlab_token(MergeRequest {
        repo: Some(gitlab_repo()),
        change: None,
    });

    let err = gw.merge(req).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn pipeline_status_missing_change_is_invalid_argument() {
    let fake = Arc::new(FakeForge::new(ForgeKind::Gitlab));
    let gw = gateway_with_repo(&fake);

    let req = with_gitlab_token(PipelineStatusRequest {
        repo: Some(gitlab_repo()),
        change: None,
    });

    let err = gw.pipeline_status(req).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn enable_auto_merge_missing_change_is_invalid_argument() {
    let fake = Arc::new(FakeForge::new(ForgeKind::Gitlab));
    let gw = gateway_with_repo(&fake);

    let req = with_gitlab_token(EnableAutoMergeRequest {
        repo: Some(gitlab_repo()),
        change: None,
    });

    let err = gw.enable_auto_merge(req).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn get_change_state_missing_change_is_invalid_argument() {
    let fake = Arc::new(FakeForge::new(ForgeKind::Gitlab));
    let gw = gateway_with_repo(&fake);

    let req = with_gitlab_token(GetChangeStateRequest {
        repo: Some(gitlab_repo()),
        change: None,
    });

    let err = gw.get_change_state(req).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

// ── 5. ReadFile on absent path → found: false, blob: None ────────────────────

#[tokio::test]
async fn read_file_absent_path_returns_not_found() {
    let fake = Arc::new(FakeForge::new(ForgeKind::Gitlab));
    let gw = gateway_with_repo(&fake);

    // The repo exists but "missing.txt" was never committed.
    let req = with_gitlab_token(ReadFileRequest {
        repo: Some(gitlab_repo()),
        path: "missing.txt".to_string(),
        r#ref: "main".to_string(),
    });

    let resp = gw.read_file(req).await.unwrap().into_inner();
    assert!(!resp.found, "absent file must have found=false");
    assert!(resp.blob.is_none(), "absent file must have blob=None");
}

// ── 6. GetChangeState always returns an empty merge_commit_sha ───────────────
//
// Documented at gateway.rs: "The trait reports state only; the merge sha is
// available via Merge." This test pins that behaviour so it can't silently
// regress.

#[tokio::test]
async fn get_change_state_merge_commit_sha_is_empty() {
    let fake = Arc::new(FakeForge::new(ForgeKind::Gitlab));
    let gw = gateway_with_change(&fake);

    let req = with_gitlab_token(GetChangeStateRequest {
        repo: Some(gitlab_repo()),
        change: Some(seeded_change()),
    });

    let resp = gw.get_change_state(req).await.unwrap().into_inner();
    assert!(
        resp.merge_commit_sha.is_empty(),
        "GetChangeState must always return an empty merge_commit_sha (quirk at gateway.rs)"
    );
}

// ── 7. ForgeError → Status::internal with message preserved ──────────────────

#[tokio::test]
async fn forge_error_becomes_internal_with_message() {
    let fake = Arc::new(FakeForge::new(ForgeKind::Gitlab));
    let gw = gateway_with_repo(&fake);

    // Inject a one-shot failure into the fake's default_branch call.
    fake.fail_next("default_branch", "something went wrong on the forge");

    let req = with_gitlab_token(GetDefaultBranchRequest {
        repo: Some(gitlab_repo()),
    });

    let err = gw.get_default_branch(req).await.unwrap_err();
    assert_eq!(
        err.code(),
        Code::Internal,
        "a ForgeError must map to Status::internal"
    );
    assert!(
        err.message().contains("something went wrong on the forge"),
        "the original error message must be preserved; got: {:?}",
        err.message()
    );
}
