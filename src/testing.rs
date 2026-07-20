//! Test doubles for the [`Forge`] contract.
//!
//! # Why this is a real module, not `#[cfg(test)]`
//!
//! `wave`, `plugin-forge`, and `geetch` are each a **separate Bazel module**, so
//! they cannot see this crate's test-only code. A `#[cfg(test)]` double would be
//! invisible to exactly the consumers that need it. It therefore lives behind
//! the non-default `testing` feature instead — the same reasoning that made
//! [`crate::ForgeError`] a concrete type rather than `anyhow`.
//!
//! # Why it implements real semantics
//!
//! [`FakeForge`] is a working in-memory forge, not a stub that returns canned
//! values. The contract's idempotency guarantees are *behavioral* — a second
//! `create_branch` returns `already_existed = true`, a second `open_change`
//! adopts the first — and a stub cannot exercise them. A double that always
//! returns `Ok(default)` would pass a conformance suite that every real adapter
//! fails.
//!
//! # Use
//!
//! ```ignore
//! let f = FakeForge::new(ForgeKind::Github);
//! f.seed_repo("acme/widgets", "main");
//! // …drive the code under test against `&f as &dyn Forge`…
//! assert!(f.calls().contains(&"open_change".to_string()));
//! ```

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::{
    default_trigger_events, repo_slug, BranchOutcome, ChangeRef, ChangeState, CiStatus,
    EnsuredTrigger, FileBlob, Forge, ForgeError, ForgeKind, ForgeResult, OpenedChange,
    PipelineStatus, RepoRef, Trigger,
};

/// One change (PR/MR) in a [`FakeRepo`].
#[derive(Debug, Clone)]
struct FakeChange {
    number: u64,
    head: String,
    base: String,
    state: ChangeState,
    /// Armed by `enable_auto_merge`; `merge` clears it.
    auto_merge: bool,
    ci: CiStatus,
    merge_sha: String,
}

/// One repository's in-memory state.
#[derive(Debug, Default, Clone)]
struct FakeRepo {
    default_branch: String,
    /// branch name → head sha.
    branches: HashMap<String, String>,
    /// (branch, path) → (content, blob_sha).
    files: HashMap<(String, String), (String, String)>,
    changes: Vec<FakeChange>,
    triggers: Vec<Trigger>,
    /// Monotonic counter for change numbers and synthetic shas.
    seq: u64,
}

/// An in-memory [`Forge`] with real idempotency semantics.
///
/// Interior-mutable so it can be shared as `&dyn Forge` without the caller
/// needing `&mut`, matching how real adapters are used.
pub struct FakeForge {
    kind: ForgeKind,
    repos: Mutex<HashMap<String, FakeRepo>>,
    calls: Mutex<Vec<String>>,
    /// One-shot error injection: the next call to the named method fails.
    /// Cleared once fired, so a test can assert recovery on the retry.
    fail_next: Mutex<Option<(String, String)>>,
}

impl FakeForge {
    #[must_use]
    pub fn new(kind: ForgeKind) -> Self {
        Self {
            kind,
            repos: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            fail_next: Mutex::new(None),
        }
    }

    /// Create `slug` with `default_branch` already pointing at a seed commit.
    ///
    /// Most contract methods are meaningless on a repository with no commits —
    /// `create_branch` has nothing to branch from and `read_file` has no tree to
    /// read. Seeding here keeps every test from repeating that setup, and
    /// mirrors why `RepoSpec.initialize` defaults true on the real thing.
    pub fn seed_repo(&self, slug: &str, default_branch: &str) {
        let mut repos = self.repos.lock().unwrap();
        let repo = repos.entry(slug.to_string()).or_default();
        repo.default_branch = default_branch.to_string();
        repo.branches
            .insert(default_branch.to_string(), "seed000000000000".to_string());
    }

    /// Every contract method called so far, in order. Lets a test assert that a
    /// code path took the cheap route (e.g. adopted an existing change rather
    /// than opening a second one).
    #[must_use]
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Fail the next call to `method` with `msg`, once.
    pub fn fail_next(&self, method: &str, msg: &str) {
        *self.fail_next.lock().unwrap() = Some((method.to_string(), msg.to_string()));
    }

    /// Record the call and fire a pending one-shot failure if it matches.
    fn enter(&self, method: &str) -> ForgeResult<()> {
        self.calls.lock().unwrap().push(method.to_string());
        let mut pending = self.fail_next.lock().unwrap();
        if pending.as_ref().is_some_and(|(m, _)| m == method) {
            let (_, msg) = pending.take().expect("checked above");
            return Err(ForgeError::msg(msg));
        }
        Ok(())
    }

    fn with_repo<T>(
        &self,
        repo: &RepoRef,
        f: impl FnOnce(&mut FakeRepo) -> ForgeResult<T>,
    ) -> ForgeResult<T> {
        let slug = repo_slug(repo);
        let mut repos = self.repos.lock().unwrap();
        let entry = repos
            .get_mut(&slug)
            .ok_or_else(|| ForgeError::msg(format!("no such repo: {slug}")))?;
        f(entry)
    }
}

#[async_trait]
impl Forge for FakeForge {
    fn kind(&self) -> ForgeKind {
        self.kind
    }

    async fn default_branch(&self, repo: &RepoRef) -> ForgeResult<String> {
        self.enter("default_branch")?;
        self.with_repo(repo, |r| Ok(r.default_branch.clone()))
    }

    async fn read_file(
        &self,
        repo: &RepoRef,
        path: &str,
        r#ref: &str,
    ) -> ForgeResult<Option<FileBlob>> {
        self.enter("read_file")?;
        self.with_repo(repo, |r| {
            let branch = if r#ref.is_empty() {
                r.default_branch.clone()
            } else {
                r#ref.to_string()
            };
            Ok(r.files
                .get(&(branch, path.to_string()))
                .map(|(content, sha)| FileBlob {
                    path: path.to_string(),
                    content: content.clone(),
                    blob_sha: sha.clone(),
                }))
        })
    }

    async fn create_branch(
        &self,
        repo: &RepoRef,
        name: &str,
        from_ref: &str,
    ) -> ForgeResult<BranchOutcome> {
        self.enter("create_branch")?;
        self.with_repo(repo, |r| {
            // Idempotent: an existing branch is NOT an error.
            if r.branches.contains_key(name) {
                return Ok(BranchOutcome {
                    created: false,
                    already_existed: true,
                });
            }
            let from = if from_ref.is_empty() {
                r.default_branch.clone()
            } else {
                from_ref.to_string()
            };
            // `from_ref` may be a branch name or a raw sha; resolve the former.
            let sha = r.branches.get(&from).cloned().unwrap_or(from);
            r.branches.insert(name.to_string(), sha);
            Ok(BranchOutcome {
                created: true,
                already_existed: false,
            })
        })
    }

    async fn commit_file(
        &self,
        repo: &RepoRef,
        branch: &str,
        path: &str,
        content: &str,
        _blob_sha: &str,
        _message: &str,
    ) -> ForgeResult<String> {
        self.enter("commit_file")?;
        self.with_repo(repo, |r| {
            if !r.branches.contains_key(branch) {
                return Err(ForgeError::msg(format!("no such branch: {branch}")));
            }
            let key = (branch.to_string(), path.to_string());
            // Byte-identical content is a no-op success, not an error: a
            // re-running cascade must be idempotent.
            if r.files.get(&key).is_some_and(|(c, _)| c == content) {
                return Ok(r.branches[branch].clone());
            }
            r.seq += 1;
            let sha = format!("commit{:010}", r.seq);
            r.files
                .insert(key, (content.to_string(), format!("blob{:012}", r.seq)));
            r.branches.insert(branch.to_string(), sha.clone());
            Ok(sha)
        })
    }

    async fn open_change(
        &self,
        repo: &RepoRef,
        head: &str,
        base: &str,
        _title: &str,
        _body: &str,
        _remove_source_branch: bool,
    ) -> ForgeResult<OpenedChange> {
        self.enter("open_change")?;
        self.with_repo(repo, |r| {
            // Idempotent: adopt an existing OPEN change for the same head.
            // A merged/closed one does not block opening a new change.
            if let Some(c) = r
                .changes
                .iter()
                .find(|c| c.head == head && c.state == ChangeState::Open)
            {
                return Ok(OpenedChange {
                    change: ChangeRef {
                        number: c.number,
                        url: format!("https://fake/{}/changes/{}", repo_slug(repo), c.number),
                        branch: c.head.clone(),
                    },
                    already_existed: true,
                });
            }
            r.seq += 1;
            let number = r.seq;
            r.changes.push(FakeChange {
                number,
                head: head.to_string(),
                base: base.to_string(),
                state: ChangeState::Open,
                auto_merge: false,
                ci: CiStatus::None,
                merge_sha: String::new(),
            });
            Ok(OpenedChange {
                change: ChangeRef {
                    number,
                    url: format!("https://fake/{}/changes/{number}", repo_slug(repo)),
                    branch: head.to_string(),
                },
                already_existed: false,
            })
        })
    }

    async fn enable_auto_merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<bool> {
        self.enter("enable_auto_merge")?;
        self.with_repo(repo, |r| {
            let c = r
                .changes
                .iter_mut()
                .find(|c| c.number == change.number)
                .ok_or_else(|| ForgeError::msg(format!("no such change: {}", change.number)))?;
            c.auto_merge = true;
            Ok(true)
        })
    }

    async fn pipeline_status(
        &self,
        repo: &RepoRef,
        change: &ChangeRef,
    ) -> ForgeResult<PipelineStatus> {
        self.enter("pipeline_status")?;
        self.with_repo(repo, |r| {
            let c = r
                .changes
                .iter()
                .find(|c| c.number == change.number)
                .ok_or_else(|| ForgeError::msg(format!("no such change: {}", change.number)))?;
            Ok(PipelineStatus {
                status: c.ci,
                pipeline_id: format!("pipe-{}", c.number),
                url: format!("https://fake/{}/pipelines/{}", repo_slug(repo), c.number),
            })
        })
    }

    async fn merge(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<String> {
        self.enter("merge")?;
        self.with_repo(repo, |r| {
            r.seq += 1;
            let sha = format!("merge{:011}", r.seq);
            let base = {
                let c = r
                    .changes
                    .iter_mut()
                    .find(|c| c.number == change.number)
                    .ok_or_else(|| ForgeError::msg(format!("no such change: {}", change.number)))?;
                if c.state != ChangeState::Open {
                    return Err(ForgeError::msg(format!(
                        "change {} is not open (state {:?})",
                        change.number, c.state
                    )));
                }
                c.state = ChangeState::Merged;
                c.auto_merge = false;
                c.merge_sha = sha.clone();
                c.base.clone()
            };
            r.branches.insert(base, sha.clone());
            Ok(sha)
        })
    }

    async fn change_state(&self, repo: &RepoRef, change: &ChangeRef) -> ForgeResult<ChangeState> {
        self.enter("change_state")?;
        self.with_repo(repo, |r| {
            r.changes
                .iter()
                .find(|c| c.number == change.number)
                .map(|c| c.state)
                .ok_or_else(|| ForgeError::msg(format!("no such change: {}", change.number)))
        })
    }

    async fn list_triggers(&self, repo: &RepoRef) -> ForgeResult<Vec<Trigger>> {
        self.enter("list_triggers")?;
        self.with_repo(repo, |r| Ok(r.triggers.clone()))
    }

    async fn ensure_trigger(
        &self,
        repo: &RepoRef,
        url: &str,
        events: &[String],
        _secret: &str,
    ) -> ForgeResult<EnsuredTrigger> {
        self.enter("ensure_trigger")?;
        self.with_repo(repo, |r| {
            // Idempotent BY URL — that is the identity a real adapter matches on.
            if let Some(t) = r.triggers.iter().find(|t| t.url == url) {
                return Ok(EnsuredTrigger {
                    trigger: t.clone(),
                    created: false,
                });
            }
            r.seq += 1;
            let trigger = Trigger {
                id: format!("hook-{}", r.seq),
                url: url.to_string(),
                events: if events.is_empty() {
                    default_trigger_events()
                } else {
                    events.to_vec()
                },
                active: true,
            };
            r.triggers.push(trigger.clone());
            Ok(EnsuredTrigger {
                trigger,
                created: true,
            })
        })
    }
}

// ── test-only helpers for driving CI state ──────────────────────────────────

impl FakeForge {
    /// Set a change's pipeline status — lets a test drive the poll-then-merge
    /// path without a real CI system.
    pub fn set_ci(&self, repo: &RepoRef, number: u64, status: CiStatus) {
        let _ = self.with_repo(repo, |r| {
            if let Some(c) = r.changes.iter_mut().find(|c| c.number == number) {
                c.ci = status;
            }
            Ok(())
        });
    }

    /// Whether auto-merge is currently armed on a change.
    #[must_use]
    pub fn auto_merge_armed(&self, repo: &RepoRef, number: u64) -> bool {
        self.with_repo(repo, |r| {
            Ok(r.changes
                .iter()
                .find(|c| c.number == number)
                .is_some_and(|c| c.auto_merge))
        })
        .unwrap_or(false)
    }
}
