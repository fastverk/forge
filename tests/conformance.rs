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
