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


// TODO(geetch): once geetch serves forge.v1, add a GeetchFixture that points at
// a test instance and `conformance_suite!(geetch, GeetchFixture::new())`. That
// single line is geetch's B2 acceptance criterion.
//
// TODO(github/gitlab): real-adapter fixtures need recorded HTTP fixtures so they
// run offline in CI. Record once against a scratch org, replay thereafter.
