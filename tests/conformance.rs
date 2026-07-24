//! Conformance run for the in-memory fake adapters.
//!
//! The suite itself lives in `forge::conformance` (a library module behind the
//! `testing` feature) so out-of-crate adapters can run the identical battery.
//! This file only supplies a fixture and invokes it.

use forge::conformance::Fixture;
use forge::testing::FakeForge;
use forge::{Forge, ForgeKind, RepoRef};

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

forge::conformance_suite!(fake_github, FakeFixture::new(ForgeKind::Github));
forge::conformance_suite!(fake_gitlab, FakeFixture::new(ForgeKind::Gitlab));

/// The same battery driven through `forge::runtime::pin`.
///
/// The decorator exists so an out-of-module consumer's runtime never has to host
/// this crate's hyper (see `forge::runtime` — it is why every `wave-discover`
/// CronJob panicked). It forwards 18 trait methods by hand, which is exactly the
/// kind of mechanical code that acquires a transposed-argument bug and reports
/// it as a wrong API call much later. Running the real contract through it
/// catches that here.
///
/// What this canNOT reproduce is the panic itself: in-crate there is only one
/// tokio, so the two-runtime mismatch cannot exist. It pins forwarding fidelity;
/// the panic is pinned by the fix's design.
struct PinnedFixture {
    fake: std::sync::Arc<FakeForge>,
    pinned: forge::runtime::Pinned,
    repo: RepoRef,
}

impl PinnedFixture {
    fn new(kind: ForgeKind) -> Self {
        let repo = RepoRef {
            forge: kind as i32,
            host: "fake.invalid".to_string(),
            owner: "acme".to_string(),
            name: "widgets".to_string(),
        };
        let fake = std::sync::Arc::new(FakeForge::new(kind));
        fake.seed_repo("acme/widgets", "main");
        let pinned = forge::runtime::pin_arc(fake.clone());
        Self { fake, pinned, repo }
    }
}

impl Fixture for PinnedFixture {
    fn forge(&self) -> &dyn Forge {
        &self.pinned
    }
    fn repo(&self) -> RepoRef {
        self.repo.clone()
    }
    fn inject_failure(&self, method: &str, msg: &str) -> bool {
        self.fake.fail_next(method, msg);
        true
    }
}

forge::conformance_suite!(pinned_github, PinnedFixture::new(ForgeKind::Github));

// geetch: DONE, and it lives in `tests/geetch_adapter.rs` rather than here.
// geetch serves forge.v1 natively, so the `forge::geetch` adapter runs this same
// suite over a real socket against a served `FakeForge`; geetch's own repo runs
// it through that adapter against a running geetchd (this module cannot build one
// — geetch bazel_deps this module, so the dependency only goes one way). It is a
// separate file because it needs an async fixture and a server, not because the
// battery differs — it is byte-for-byte the same eight cases.
//
// TODO(github/gitlab): real-adapter fixtures need recorded HTTP fixtures so they
// run offline in CI. Record once against a scratch org, replay thereafter.
