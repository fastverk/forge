//! [`ForgeProvisioner`] — creating and configuring repositories.
//!
//! # Why this is a separate trait, not more methods on [`Forge`]
//!
//! The obvious move was to add default-bodied methods to [`Forge`], following
//! the `list_triggers` / `ensure_trigger` precedent. That precedent is for
//! *additive operations in the same privilege class*, and provisioning is not
//! in that class:
//!
//! - **Credentials differ.** Read/write is a user PAT or an App-installation
//!   token; creating a repository is org-admin. One constructor cannot hold both
//!   honestly, and pretending otherwise means the powerful credential is always
//!   present.
//! - **The gateway's whole model breaks.** `forge-gateway` holds *no* credential
//!   and runs every operation as the caller. Provisioning needs an identity the
//!   server itself holds. Co-serving them means one pod compromise yields both.
//! - **Type-level blast radius.** Every `Box<dyn Forge>` in `wave` — a
//!   standalone developer CLI — would become statically capable of
//!   `delete_repo`. That is not a capability a cascade engine should carry.
//!
//! There is deliberately **no** `ForgeProvisioner: Forge` supertrait either.
//! An implementation may compose a [`Forge`] internally (the seed commit in
//! `ensure_repo` needs one), but composition is not a bound — requiring both on
//! one object reintroduces the single-credential problem.
//!
//! # Idempotence
//!
//! Every `ensure_*` converges rather than creates. The caller is a reconcile
//! loop that will call repeatedly; a second call against the desired state is a
//! no-op success, not an error.
//!
//! # Honesty about what a forge cannot do
//!
//! [`Protection::unsupported`] is the load-bearing field in this module. Forges
//! differ sharply on branch protection — some have no status checks to require;
//! some enforce force-push bans through out-of-band policy rather than an API.
//! An adapter that cannot honor part of a [`ProtectionSpec`] **reports it**
//! rather than returning success. A caller who believes a branch is protected
//! and is wrong is worse off than one who knows it isn't.

use async_trait::async_trait;

pub use crate::pb::{
    BusSink, Delivery, DeliverySink, EnsuredDelivery, Protection, ProtectionSpec, ProvisionedRepo,
    RepoLifecycle, RepoSpec, Visibility, WebhookSink,
};
use crate::{ForgeError, ForgeKind, ForgeResult, RepoRef};

/// Outcome of [`ForgeProvisioner::ensure_repo`] — idempotent over an existing repo.
#[derive(Debug, Clone)]
pub struct EnsuredRepo {
    pub repo: ProvisionedRepo,
    /// False when the repository already existed.
    pub created: bool,
}

/// A [`RepoSpec`] with the defaults callers should almost always want.
///
/// `initialize` is **true**: most of the [`Forge`](crate::Forge) contract is
/// meaningless on an empty repository — there is no commit to branch from and no
/// tree to read — so an unseeded repo fails two calls later, far from the cause.
#[must_use]
pub fn default_repo_spec() -> RepoSpec {
    RepoSpec {
        description: String::new(),
        visibility: Visibility::Private as i32,
        default_branch: "main".to_string(),
        initialize: true,
        tags: Default::default(),
    }
}

/// Repository lifecycle + configuration, for a caller holding a privileged
/// identity.
#[async_trait]
pub trait ForgeProvisioner: Send + Sync {
    /// Which forge this provisioner targets.
    fn kind(&self) -> ForgeKind;

    /// Converge `repo` onto `spec`. Idempotent: an existing repository is
    /// returned with `created = false`, not an error.
    async fn ensure_repo(&self, repo: &RepoRef, spec: &RepoSpec) -> ForgeResult<EnsuredRepo>;

    /// The repository as it exists, or `Ok(None)` when absent.
    ///
    /// Absence is a normal answer, not a failure — callers branch on it.
    async fn describe_repo(&self, repo: &RepoRef) -> ForgeResult<Option<ProvisionedRepo>>;

    /// Make `repo` read-only, preserving its contents.
    ///
    /// Not every forge has a native archive; an adapter may implement this by
    /// other means, but it must genuinely stop writes rather than only labeling
    /// the repository.
    async fn archive_repo(&self, repo: &RepoRef) -> ForgeResult<()>;

    /// **Destructive.** Errors unless `confirm_name == repo.name`.
    ///
    /// The guard is not ceremony: not every forge has a recycle bin, so on some
    /// this is irreversible. Implementations should also tear down deliveries
    /// and protection first — a leaked delivery rule outlives its repository and
    /// fails forever.
    async fn delete_repo(&self, repo: &RepoRef, confirm_name: &str) -> ForgeResult<()>;

    /// Converge `branch` onto `spec`.
    ///
    /// Anything the forge cannot honor MUST come back in
    /// [`Protection::unsupported`] — never silently dropped.
    async fn ensure_protection(
        &self,
        repo: &RepoRef,
        branch: &str,
        spec: &ProtectionSpec,
    ) -> ForgeResult<Protection>;

    /// Protection as it exists on `branch`, or `Ok(None)` when unprotected.
    async fn describe_protection(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> ForgeResult<Option<Protection>>;

    /// Idempotently ensure `repo` delivers events to `sink`.
    ///
    /// The forge-neutral superset of [`Forge::ensure_trigger`]: a webhook where
    /// the forge has them, something else where it doesn't. An adapter asked for
    /// a [`WebhookSink`] it cannot provide must **error**, not quietly
    /// substitute a different delivery mechanism — a caller expecting an HTTPS
    /// POST and silently given a queue subscription will wait forever for
    /// deliveries that are going somewhere else.
    ///
    /// [`Forge::ensure_trigger`]: crate::Forge::ensure_trigger
    async fn ensure_delivery(
        &self,
        repo: &RepoRef,
        sink: &DeliverySink,
    ) -> ForgeResult<EnsuredDelivery>;

    /// Every delivery configured on `repo`.
    async fn list_deliveries(&self, repo: &RepoRef) -> ForgeResult<Vec<Delivery>>;

    /// Remove one delivery. Idempotent: removing an absent id is a no-op success.
    async fn remove_delivery(&self, repo: &RepoRef, id: &str) -> ForgeResult<()>;
}

/// The error an adapter returns for a capability it genuinely cannot provide.
///
/// Prefer this over a plausible-looking success. Naming the operation and the
/// forge is what turns "it didn't work" into "this forge has no such concept".
#[must_use]
pub fn unsupported(op: &str, kind: ForgeKind) -> ForgeError {
    ForgeError::msg(format!("{op} is not supported by the {kind:?} provisioner"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seeding is on by default — an unseeded repo breaks `create_branch` and
    /// `read_file` two calls later, far from the cause.
    #[test]
    fn default_spec_initializes_and_is_private() {
        let s = default_repo_spec();
        assert!(s.initialize, "a repo with no commit has no default branch");
        assert_eq!(s.visibility, Visibility::Private as i32);
        assert_eq!(s.default_branch, "main");
    }

    #[test]
    fn unsupported_names_the_op_and_the_forge() {
        let e = unsupported("ensure_protection", ForgeKind::Geetch);
        let msg = e.to_string();
        assert!(msg.contains("ensure_protection"), "{msg}");
        assert!(msg.contains("Geetch"), "{msg}");
    }
}
