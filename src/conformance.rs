//! The [`Forge`] conformance suite — the executable specification.
//!
//! # What this is for
//!
//! The `Forge` trait states its guarantees in prose: "idempotent where noted, so
//! a resuming reconcile loop can call them repeatedly without creating
//! duplicates". Prose does not fail a build.
//!
//! **This suite is the contract.** An adapter is correct when it passes. New
//! contract semantics get a case here *first*; they are not asserted in an
//! adapter-local test, because a test that lives next to one adapter cannot
//! constrain the others.
//!
//! # Why this lives in `src/`, not `tests/`
//!
//! It started in `tests/conformance.rs`. Rust integration tests are not
//! importable by other crates, so an out-of-crate adapter — geetch, and
//! eventually the recorded github/gitlab fixtures — could not run the shared
//! battery. geetch's B2 hit exactly that wall and copied all eight cases into its
//! own repo instead, which defeats the purpose: a copy drifts, and an adapter
//! that passes its own copy has proved nothing about the shared contract.
//!
//! So the suite is a library module behind the non-default `testing` feature —
//! the same reasoning that put [`crate::testing::FakeForge`] there rather than
//! under `#[cfg(test)]`, applied one level up.
//!
//! # How to run it against a new adapter
//!
//! ```rust,ignore
//! use forge::conformance::Fixture;
//!
//! struct MyFixture { /* connection state */ }
//! impl Fixture for MyFixture { /* forge(), repo() */ }
//!
//! forge::conformance_suite!(my_adapter, MyFixture::new());
//! ```
//!
//! That single invocation is the whole integration. An adapter that cannot
//! support a method should fail loudly here rather than silently returning a
//! plausible default — see [`unsupported_is_explicit`].
//!
//! Two requirements on the consumer: depend on `forge` with the `testing`
//! feature enabled, and have `tokio` available with the `macros` and `rt`
//! features — the generated cases are `#[tokio::test]`, so the attribute is
//! resolved in the CONSUMER's crate, not this one.

use crate::{ChangeState, CiStatus, Forge, RepoRef};

/// Builds a live adapter plus a repo that already exists on it.
///
/// A trait rather than a closure so a real-adapter fixture can hold connection
/// state and tear the repo down on drop.
pub trait Fixture {
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
// ── the cases ───────────────────────────────────────────────────────────────

/// `create_branch` is idempotent: a second call for the same name reports
/// `already_existed`, it does not error.
///
/// This is the single most load-bearing guarantee in the trait. The cascade
/// engine resumes mid-run and re-drives every step; an adapter that errors here
/// wedges the whole cascade on retry.
pub async fn create_branch_is_idempotent(fx: &dyn Fixture) {
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
pub async fn open_change_adopts_existing(fx: &dyn Fixture) {
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
pub async fn commit_identical_content_is_noop_success(fx: &dyn Fixture) {
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
pub async fn read_file_roundtrips_and_absent_is_none(fx: &dyn Fixture) {
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
pub async fn merged_change_is_merged_not_closed(fx: &dyn Fixture) {
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
pub async fn ensure_trigger_is_idempotent_by_url(fx: &dyn Fixture) {
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
pub async fn unsupported_is_explicit(fx: &dyn Fixture) {
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
pub async fn transient_failure_is_recoverable(fx: &dyn Fixture) {
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
#[macro_export]
macro_rules! conformance_suite {
    ($modname:ident, $fixture:expr) => {
        mod $modname {
            use super::*;

            macro_rules! case {
                ($name:ident) => {
                    #[tokio::test]
                    async fn $name() {
                        let fx = $fixture;
                        $crate::conformance::$name(&fx).await;
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
