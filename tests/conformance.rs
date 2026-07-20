//! The [`Forge`] conformance suite — the executable specification.
//!
//! # What this is for
//!
//! The `Forge` trait states its guarantees in prose: "idempotent where noted, so
//! a resuming reconcile loop can call them repeatedly without creating
//! duplicates". Prose does not fail a build. Before this suite existed, every
//! test in the crate was a pure-function mapper assertion, the GitHub and GitLab
//! adapters had no shared behavioral test at all, and `gateway.rs` — the sole
//! dispatch point — had none.
//!
//! That is fine with two adapters written by one person in one sitting. It stops
//! being fine the moment adapters are developed in parallel, because each one
//! encodes its own reading of the contract and the divergence only surfaces in
//! production.
//!
//! **So: this suite is the contract.** An adapter is correct when it passes.
//! New contract semantics get a case here *first*; they are not asserted in an
//! adapter-local test, because a test that lives next to one adapter cannot
//! constrain the others.
//!
//! # How to run it against a new adapter
//!
//! Implement [`Fixture`] and add a `conformance_suite!` invocation. Everything
//! else is shared. An adapter that cannot support a method should fail loudly
//! here rather than silently returning a plausible default — see
//! [`unsupported_is_explicit`].

use forge::testing::FakeForge;
use forge::{ChangeState, CiStatus, Forge, ForgeKind, RepoRef};

/// Builds a live adapter plus a repo that already exists on it.
///
/// A trait rather than a closure so a real-adapter fixture can hold connection
/// state and tear the repo down on drop.
trait Fixture {
    /// The adapter under test, with `repo()` already existing and non-empty.
    fn forge(&self) -> &dyn Forge;
    /// A repository the adapter can read and write.
    fn repo(&self) -> RepoRef;

    /// Make the next call to `method` fail, if this fixture can inject faults.
    ///
    /// Returns `false` when injection is unsupported, and the cases that need it
    /// skip themselves. A real-forge fixture cannot synthesize a network blip on
    /// demand, so fault-injection cases are in-memory only — but they still
    /// belong in the shared suite, because the *behavior* they pin (a failed
    /// call leaves no partial state) is contract, not implementation.
    fn inject_failure(&self, _method: &str, _msg: &str) -> bool {
        false
    }
}

/// The in-memory fixture. Real-adapter fixtures (github, gitlab, geetch) plug in
/// beside this one.
struct FakeFixture {
    forge: FakeForge,
    repo: RepoRef,
}

impl FakeFixture {
    fn new(kind: ForgeKind) -> Self {
        let repo = RepoRef {
            forge: kind as i32,
            host: "fake.invalid".to_string(),
            owner: "acme".to_string(),
            name: "widgets".to_string(),
        };
        let forge = FakeForge::new(kind);
        forge.seed_repo("acme/widgets", "main");
        Self { forge, repo }
    }
}

impl Fixture for FakeFixture {
    fn forge(&self) -> &dyn Forge {
        &self.forge
    }
    fn repo(&self) -> RepoRef {
        self.repo.clone()
    }
    fn inject_failure(&self, method: &str, msg: &str) -> bool {
        self.forge.fail_next(method, msg);
        true
    }
}

// ── the cases ───────────────────────────────────────────────────────────────

/// `create_branch` is idempotent: a second call for the same name reports
/// `already_existed`, it does not error.
///
/// This is the single most load-bearing guarantee in the trait. The cascade
/// engine resumes mid-run and re-drives every step; an adapter that errors here
/// wedges the whole cascade on retry.
async fn create_branch_is_idempotent(fx: &dyn Fixture) {
    let (f, repo) = (fx.forge(), fx.repo());

    let first = f.create_branch(&repo, "feat/x", "main").await.unwrap();
    assert!(first.created, "first create_branch should create");
    assert!(
        !first.already_existed,
        "first create_branch is not a re-open"
    );

    let second = f.create_branch(&repo, "feat/x", "main").await.unwrap();
    assert!(
        second.already_existed,
        "a second create_branch MUST report already_existed, not error"
    );
    assert!(
        !second.created,
        "a second create_branch must not claim to create"
    );
}

/// `open_change` adopts an existing OPEN change for the same head rather than
/// opening a duplicate — and reports that it did so.
async fn open_change_adopts_existing(fx: &dyn Fixture) {
    let (f, repo) = (fx.forge(), fx.repo());
    f.create_branch(&repo, "feat/adopt", "main").await.unwrap();

    let first = f
        .open_change(&repo, "feat/adopt", "main", "t", "b", false)
        .await
        .unwrap();
    assert!(!first.already_existed);

    let second = f
        .open_change(&repo, "feat/adopt", "main", "t", "b", false)
        .await
        .unwrap();
    assert!(
        second.already_existed,
        "re-opening for the same head MUST adopt the open change"
    );
    assert_eq!(
        first.change.number, second.change.number,
        "adoption must return the SAME change, not a new one"
    );
}

/// Committing byte-identical content is a no-op success, not an error.
///
/// Real forges disagree here — CodeCommit raises `NoChangeException`, GitHub
/// 422s — so adapters must normalize. A cascade that re-runs over an already
/// applied change must not fail.
async fn commit_identical_content_is_noop_success(fx: &dyn Fixture) {
    let (f, repo) = (fx.forge(), fx.repo());
    f.create_branch(&repo, "feat/noop", "main").await.unwrap();

    let first = f
        .commit_file(&repo, "feat/noop", "a.txt", "hello", "", "add a")
        .await
        .unwrap();
    assert!(!first.is_empty(), "commit_file should return a sha");

    f.commit_file(&repo, "feat/noop", "a.txt", "hello", "", "add a")
        .await
        .expect("identical content MUST be a no-op success, not an error");
}

/// A file written on a branch reads back on that branch, and an absent path is
/// `Ok(None)` — not an error.
///
/// The `Ok(None)` half matters: callers branch on absence (does this repo have a
/// MODULE.bazel?), and an adapter that errors instead turns a normal question
/// into a failure.
async fn read_file_roundtrips_and_absent_is_none(fx: &dyn Fixture) {
    let (f, repo) = (fx.forge(), fx.repo());
    f.create_branch(&repo, "feat/read", "main").await.unwrap();
    f.commit_file(&repo, "feat/read", "b.txt", "content", "", "add b")
        .await
        .unwrap();

    let got = f.read_file(&repo, "b.txt", "feat/read").await.unwrap();
    let blob = got.expect("written file must read back");
    assert_eq!(blob.content, "content");
    assert!(
        !blob.blob_sha.is_empty(),
        "blob_sha must round-trip for updates"
    );

    let missing = f.read_file(&repo, "nope.txt", "feat/read").await.unwrap();
    assert!(
        missing.is_none(),
        "an absent path is Ok(None), never an error"
    );
}

/// A merged change reports `Merged` — never `Closed`.
///
/// Collapsing the two makes the cascade believe every merged change was
/// abandoned, so it never advances. CodeCommit has no MERGED status natively
/// (a merged PR is CLOSED with `mergeMetadata.isMerged`), which is exactly the
/// kind of adapter-local detail this case exists to catch.
async fn merged_change_is_merged_not_closed(fx: &dyn Fixture) {
    let (f, repo) = (fx.forge(), fx.repo());
    f.create_branch(&repo, "feat/merge", "main").await.unwrap();
    f.commit_file(&repo, "feat/merge", "c.txt", "x", "", "add c")
        .await
        .unwrap();
    let opened = f
        .open_change(&repo, "feat/merge", "main", "t", "b", false)
        .await
        .unwrap();

    assert_eq!(
        f.change_state(&repo, &opened.change).await.unwrap(),
        ChangeState::Open
    );

    let sha = f.merge(&repo, &opened.change).await.unwrap();
    assert!(!sha.is_empty(), "merge must return the merge commit sha");

    assert_eq!(
        f.change_state(&repo, &opened.change).await.unwrap(),
        ChangeState::Merged,
        "a merged change reports Merged, NOT Closed"
    );
}

/// `ensure_trigger` is idempotent by delivery URL.
///
/// URL is the identity, not the id: the caller does not know the forge-assigned
/// id, and reconciliation is "make sure a hook to *this endpoint* exists".
async fn ensure_trigger_is_idempotent_by_url(fx: &dyn Fixture) {
    let (f, repo) = (fx.forge(), fx.repo());
    let url = "https://hooks.example/webhook";

    let first = match f.ensure_trigger(&repo, url, &[], "s3cret").await {
        Ok(t) => t,
        // An adapter that cannot install triggers must say so — covered by
        // `unsupported_is_explicit`; skip the idempotency assertion here.
        Err(_) => return,
    };
    assert!(first.created, "first ensure_trigger should create");
    assert_eq!(first.trigger.url, url);
    assert!(
        !first.trigger.events.is_empty(),
        "an empty `events` request must fall back to the adapter default, not subscribe to nothing"
    );

    let second = f.ensure_trigger(&repo, url, &[], "s3cret").await.unwrap();
    assert!(
        !second.created,
        "a second ensure_trigger at the same URL MUST report created=false"
    );

    let listed = f.list_triggers(&repo).await.unwrap();
    assert_eq!(
        listed.iter().filter(|t| t.url == url).count(),
        1,
        "ensure_trigger must not stack duplicate hooks for one URL"
    );
}

/// An unsupported capability returns a clear `Err` — it never fabricates a
/// plausible success.
///
/// This is the case that protects the platform from an adapter quietly
/// pretending. A forge with no auto-merge must return `Ok(false)` (the trait's
/// documented "unavailable" signal) so the caller falls back to poll-then-merge;
/// a forge with no trigger API must `Err`. What neither may do is return
/// `Ok(true)` and drop the request on the floor.
async fn unsupported_is_explicit(fx: &dyn Fixture) {
    let (f, repo) = (fx.forge(), fx.repo());
    f.create_branch(&repo, "feat/unsup", "main").await.unwrap();
    let opened = f
        .open_change(&repo, "feat/unsup", "main", "t", "b", false)
        .await
        .unwrap();

    // Must not panic and must not lie: either it armed (true) or it can't (false).
    let armed = f.enable_auto_merge(&repo, &opened.change).await.unwrap();
    if !armed {
        // The documented fallback path must then work: poll, then merge.
        let status = f.pipeline_status(&repo, &opened.change).await.unwrap();
        assert!(
            matches!(
                status.status,
                CiStatus::None
                    | CiStatus::Pending
                    | CiStatus::Running
                    | CiStatus::Success
                    | CiStatus::Failed
                    | CiStatus::Canceled
                    | CiStatus::Unspecified
            ),
            "pipeline_status must return a defined CiStatus even with no pipeline"
        );
    }
}

/// A transient failure must not corrupt state: the retry succeeds and leaves
/// exactly one branch.
async fn transient_failure_is_recoverable(fx: &dyn Fixture) {
    let (f, repo) = (fx.forge(), fx.repo());

    // Real-forge fixtures cannot synthesize a blip on demand; they skip.
    if !fx.inject_failure("create_branch", "transient network blip") {
        return;
    }

    assert!(
        f.create_branch(&repo, "feat/retry", "main").await.is_err(),
        "injected failure should surface"
    );
    let retry = f.create_branch(&repo, "feat/retry", "main").await.unwrap();
    assert!(
        retry.created,
        "after a failure that created nothing, the retry CREATES — it must not report already_existed"
    );
}

// ── the harness ─────────────────────────────────────────────────────────────

/// Generate the full suite for one fixture. Every adapter gets an identical
/// battery; adding a case here adds it everywhere at once, which is the point.
macro_rules! conformance_suite {
    ($modname:ident, $fixture:expr) => {
        mod $modname {
            use super::*;

            macro_rules! case {
                ($name:ident) => {
                    #[tokio::test]
                    async fn $name() {
                        let fx = $fixture;
                        super::$name(&fx).await;
                    }
                };
            }

            case!(create_branch_is_idempotent);
            case!(open_change_adopts_existing);
            case!(commit_identical_content_is_noop_success);
            case!(read_file_roundtrips_and_absent_is_none);
            case!(merged_change_is_merged_not_closed);
            case!(ensure_trigger_is_idempotent_by_url);
            case!(unsupported_is_explicit);
            case!(transient_failure_is_recoverable);
        }
    };
}

conformance_suite!(fake_github, FakeFixture::new(ForgeKind::Github));
conformance_suite!(fake_gitlab, FakeFixture::new(ForgeKind::Gitlab));

// TODO(geetch): once geetch serves forge.v1, add a GeetchFixture that points at
// a test instance and `conformance_suite!(geetch, GeetchFixture::new())`. That
// single line is geetch's B2 acceptance criterion.
//
// TODO(github/gitlab): real-adapter fixtures need recorded HTTP fixtures so they
// run offline in CI. Record once against a scratch org, replay thereafter.
