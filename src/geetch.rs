//! `geetch` — drive a running geetchd as a [`Forge`] and [`ForgeProvisioner`].
//!
//! geetch is the platform's own forge: it SERVES `forge.v1.ForgeService` and
//! `forge.v1.ForgeProvisionService` natively, rather than mapping the contract
//! onto someone else's REST API. So this adapter is a near-1:1 gRPC passthrough
//! — build the request, call the stub, unwrap the response — and there is
//! deliberately no mapping table in this file. If one ever appears here, geetch
//! has drifted from the contract it is supposed to implement exactly.
//!
//! # Why this is trivial where [`github`] and [`gitlab`] are not
//!
//! Those adapters translate: GitHub's blob shas, GitLab's `last_commit_id`, two
//! different notions of "merge when pipeline succeeds". Each loses a little
//! fidelity. geetch defines its own storage against this contract, so the trait
//! types ARE the wire types here — `RepoRef`, `ChangeRef`, `Trigger` and friends
//! are re-exports of [`crate::pb`], so a request is constructed, not converted.
//!
//! [`github`]: crate::github
//! [`gitlab`]: crate::gitlab
//!
//! # Two clients, two privilege classes
//!
//! [`GeetchForge`] speaks `ForgeService` — the per-caller surface, which holds no
//! credential of its own. [`GeetchProvisioner`] speaks `ForgeProvisionService`,
//! which needs a privileged identity the server holds and is served on a
//! SEPARATE listener geetchd only binds when an operator opts in. They are
//! separate types on purpose: a `Box<dyn Forge>` in a developer CLI must not be
//! statically capable of deleting repositories. Do not merge them.
//!
//! # Connection
//!
//! Both dial lazily per call via a cloned `Channel`, which is cheap — tonic's
//! `Channel` is a handle over a shared connection pool, so cloning it does not
//! open a socket.

use async_trait::async_trait;
use tonic::transport::Channel;

use crate::pb::{
    forge_provision_service_client::ForgeProvisionServiceClient,
    forge_service_client::ForgeServiceClient, ArchiveRepoRequest, CommitFileRequest,
    CreateBranchRequest, DeleteRepoRequest, DescribeProtectionRequest, DescribeRepoRequest,
    EnableAutoMergeRequest, EnsureDeliveryRequest, EnsureProtectionRequest, EnsureRepoRequest,
    EnsureTriggerRequest, GetChangeStateRequest, GetDefaultBranchRequest, ListDeliveriesRequest,
    ListTriggersRequest, MergeRequest, OpenChangeRequest, PipelineStatusRequest, ReadFileRequest,
    RemoveDeliveryRequest,
};
use crate::provision::{
    Delivery, DeliverySink, EnsuredDelivery, EnsuredRepo, ForgeProvisioner, Protection,
    ProtectionSpec, ProvisionedRepo, RepoSpec,
};
use crate::{
    BranchOutcome, ChangeRef, ChangeState, CiStatus, EnsuredTrigger, FileBlob, Forge, ForgeError,
    ForgeKind, ForgeResult, OpenedChange, PipelineStatus, RepoRef, Trigger,
};

/// A gRPC status becomes a [`ForgeError`] carrying the server's message. The
/// code is dropped deliberately: the trait's error type is opaque, and callers
/// branch on the *operation*, not on a transport code.
fn err(s: tonic::Status) -> ForgeError {
    ForgeError::msg(s.message())
}

/// Connect a [`Channel`] to a geetchd endpoint (e.g. `http://geetchd:50057`).
///
/// Shared by both clients. A bare `host:port` is accepted and given an `http://`
/// scheme, since tonic requires one.
pub async fn connect(addr: &str) -> ForgeResult<Channel> {
    let endpoint = if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };
    Channel::from_shared(endpoint.clone())
        .map_err(|e| ForgeError::msg(format!("invalid geetch address {addr:?}: {e}")))?
        .connect()
        .await
        .map_err(|e| ForgeError::msg(format!("could not reach geetch at {endpoint}: {e}")))
}

// ── the Forge adapter ───────────────────────────────────────────────────────────

/// A [`Forge`] backed by a running geetchd's `forge.v1.ForgeService`.
///
/// The network twin of the in-process adapter geetch's own conformance suite
/// uses: same mapping, one socket.
#[derive(Clone, Debug)]
pub struct GeetchForge {
    channel: Channel,
}

impl GeetchForge {
    /// Wrap an existing channel (see [`connect`]).
    #[must_use]
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    /// Connect to `addr` and wrap the result.
    pub async fn connect(addr: &str) -> ForgeResult<Self> {
        Ok(Self::new(connect(addr).await?))
    }

    fn client(&self) -> ForgeServiceClient<Channel> {
        ForgeServiceClient::new(self.channel.clone())
    }
}

#[async_trait]
impl Forge for GeetchForge {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Geetch
    }

    async fn default_branch(&self, repo: &RepoRef) -> ForgeResult<String> {
        Ok(self
            .client()
            .get_default_branch(GetDefaultBranchRequest {
                repo: Some(repo.clone()),
            })
            .await
            .map_err(err)?
            .into_inner()
            .branch)
    }

    async fn read_file(
        &self,
        repo: &RepoRef,
        path: &str,
        r#ref: &str,
    ) -> ForgeResult<Option<FileBlob>> {
        // `found = false` is a normal answer, not an error — the contract has
        // callers branch on absence.
        Ok(self
            .client()
            .read_file(ReadFileRequest {
                repo: Some(repo.clone()),
                path: path.to_string(),
                r#ref: r#ref.to_string(),
            })
            .await
            .map_err(err)?
            .into_inner()
            .blob)
    }

    async fn create_branch(
        &self,
        repo: &RepoRef,
        name: &str,
        from_ref: &str,
    ) -> ForgeResult<BranchOutcome> {
        let r = self
            .client()
            .create_branch(CreateBranchRequest {
                repo: Some(repo.clone()),
                name: name.to_string(),
                from_sha: from_ref.to_string(),
            })
            .await
            .map_err(err)?
            .into_inner();
        Ok(BranchOutcome {
            created: r.created,
            already_existed: r.already_existed,
        })
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
        Ok(self
            .client()
            .commit_file(CommitFileRequest {
                repo: Some(repo.clone()),
                branch: branch.to_string(),
                path: path.to_string(),
                content: content.to_string(),
                blob_sha: blob_sha.to_string(),
                message: message.to_string(),
            })
            .await
            .map_err(err)?
            .into_inner()
            .commit_sha)
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
        let r = self
            .client()
            .open_change(OpenChangeRequest {
                repo: Some(repo.clone()),
                head: head.to_string(),
                base: base.to_string(),
                title: title.to_string(),
                body: body.to_string(),
                remove_source_branch,
            })
            .await
            .map_err(err)?
            .into_inner();
        Ok(OpenedChange {
            change: r.change.unwrap_or_default(),
            already_existed: r.already_existed,
        })
    }

    async fn enable_auto_merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<bool> {
        Ok(self
            .client()
            .enable_auto_merge(EnableAutoMergeRequest {
                repo: Some(repo.clone()),
                change: Some(change.clone()),
            })
            .await
            .map_err(err)?
            .into_inner()
            .enabled)
    }

    async fn pipeline_status(
        &self,
        repo: &RepoRef,
        change: &ChangeRef,
    ) -> ForgeResult<PipelineStatus> {
        let r = self
            .client()
            .pipeline_status(PipelineStatusRequest {
                repo: Some(repo.clone()),
                change: Some(change.clone()),
            })
            .await
            .map_err(err)?
            .into_inner();
        Ok(PipelineStatus {
            status: CiStatus::try_from(r.status).unwrap_or(CiStatus::Unspecified),
            pipeline_id: r.pipeline_id,
            url: r.pipeline_url,
        })
    }

    async fn merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<String> {
        Ok(self
            .client()
            .merge(MergeRequest {
                repo: Some(repo.clone()),
                change: Some(change.clone()),
            })
            .await
            .map_err(err)?
            .into_inner()
            .merge_commit_sha)
    }

    async fn change_state(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<ChangeState> {
        let r = self
            .client()
            .get_change_state(GetChangeStateRequest {
                repo: Some(repo.clone()),
                change: Some(change.clone()),
            })
            .await
            .map_err(err)?
            .into_inner();
        Ok(ChangeState::try_from(r.state).unwrap_or(ChangeState::Unspecified))
    }

    async fn list_triggers(&self, repo: &RepoRef) -> ForgeResult<Vec<Trigger>> {
        Ok(self
            .client()
            .list_triggers(ListTriggersRequest {
                repo: Some(repo.clone()),
            })
            .await
            .map_err(err)?
            .into_inner()
            .triggers)
    }

    async fn ensure_trigger(
        &self,
        repo: &RepoRef,
        url: &str,
        events: &[String],
        secret: &str,
    ) -> ForgeResult<EnsuredTrigger> {
        let r = self
            .client()
            .ensure_trigger(EnsureTriggerRequest {
                repo: Some(repo.clone()),
                url: url.to_string(),
                events: events.to_vec(),
                secret: secret.to_string(),
            })
            .await
            .map_err(err)?
            .into_inner();
        Ok(EnsuredTrigger {
            trigger: r.trigger.unwrap_or_default(),
            created: r.created,
        })
    }
}

// ── the ForgeProvisioner adapter ────────────────────────────────────────────────

/// A [`ForgeProvisioner`] backed by a running geetchd's
/// `forge.v1.ForgeProvisionService`.
///
/// Deliberately a SEPARATE type from [`GeetchForge`], and pointed at whichever
/// address serves the privileged listener (geetchd binds it only when
/// `GEETCH_PROVISION_BIND` is set). Holding one of these means holding the
/// authority to create and delete repositories.
#[derive(Clone, Debug)]
pub struct GeetchProvisioner {
    channel: Channel,
}

impl GeetchProvisioner {
    /// Wrap an existing channel (see [`connect`]).
    #[must_use]
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    /// Connect to `addr` — the PRIVILEGED provisioning listener, not the
    /// per-caller `ForgeService` one.
    pub async fn connect(addr: &str) -> ForgeResult<Self> {
        Ok(Self::new(connect(addr).await?))
    }

    fn client(&self) -> ForgeProvisionServiceClient<Channel> {
        ForgeProvisionServiceClient::new(self.channel.clone())
    }
}

#[async_trait]
impl ForgeProvisioner for GeetchProvisioner {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Geetch
    }

    async fn ensure_repo(&self, repo: &RepoRef, spec: &RepoSpec) -> ForgeResult<EnsuredRepo> {
        let r = self
            .client()
            .ensure_repo(EnsureRepoRequest {
                repo: Some(repo.clone()),
                spec: Some(spec.clone()),
            })
            .await
            .map_err(err)?
            .into_inner();
        Ok(EnsuredRepo {
            repo: r.repo.unwrap_or_default(),
            created: r.created,
        })
    }

    async fn describe_repo(&self, repo: &RepoRef) -> ForgeResult<Option<ProvisionedRepo>> {
        // Absence is `found = false`, a normal answer the caller branches on.
        Ok(self
            .client()
            .describe_repo(DescribeRepoRequest {
                repo: Some(repo.clone()),
            })
            .await
            .map_err(err)?
            .into_inner()
            .repo)
    }

    async fn archive_repo(&self, repo: &RepoRef) -> ForgeResult<()> {
        self.client()
            .archive_repo(ArchiveRepoRequest {
                repo: Some(repo.clone()),
            })
            .await
            .map_err(err)?;
        Ok(())
    }

    async fn delete_repo(&self, repo: &RepoRef, confirm_name: &str) -> ForgeResult<()> {
        // The confirm_name guard is enforced SERVER-side; passing it through
        // rather than pre-checking keeps one authority for a destructive call.
        self.client()
            .delete_repo(DeleteRepoRequest {
                repo: Some(repo.clone()),
                confirm_name: confirm_name.to_string(),
            })
            .await
            .map_err(err)?;
        Ok(())
    }

    async fn ensure_protection(
        &self,
        repo: &RepoRef,
        branch: &str,
        spec: &ProtectionSpec,
    ) -> ForgeResult<Protection> {
        Ok(self
            .client()
            .ensure_protection(EnsureProtectionRequest {
                repo: Some(repo.clone()),
                branch: branch.to_string(),
                spec: Some(spec.clone()),
            })
            .await
            .map_err(err)?
            .into_inner()
            .protection
            .unwrap_or_default())
    }

    async fn describe_protection(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> ForgeResult<Option<Protection>> {
        Ok(self
            .client()
            .describe_protection(DescribeProtectionRequest {
                repo: Some(repo.clone()),
                branch: branch.to_string(),
            })
            .await
            .map_err(err)?
            .into_inner()
            .protection)
    }

    async fn ensure_delivery(
        &self,
        repo: &RepoRef,
        sink: &DeliverySink,
    ) -> ForgeResult<EnsuredDelivery> {
        Ok(self
            .client()
            .ensure_delivery(EnsureDeliveryRequest {
                repo: Some(repo.clone()),
                sink: Some(sink.clone()),
            })
            .await
            .map_err(err)?
            .into_inner()
            .delivery
            .unwrap_or_default())
    }

    async fn list_deliveries(&self, repo: &RepoRef) -> ForgeResult<Vec<Delivery>> {
        Ok(self
            .client()
            .list_deliveries(ListDeliveriesRequest {
                repo: Some(repo.clone()),
            })
            .await
            .map_err(err)?
            .into_inner()
            .deliveries)
    }

    async fn remove_delivery(&self, repo: &RepoRef, id: &str) -> ForgeResult<()> {
        self.client()
            .remove_delivery(RemoveDeliveryRequest {
                repo: Some(repo.clone()),
                id: id.to_string(),
            })
            .await
            .map_err(err)?;
        Ok(())
    }
}
