# forge

Generic forge contract (`ForgeService`) + GitHub/GitLab adapters.

The operations a code-hosting *forge* (GitHub, GitLab, …) provides that a
cross-repo automation needs, modeled **once** as a proto service so a single
contract covers every host:

- read a repo's default branch + a file;
- create a branch, commit a file;
- open a change (PR/MR), enable auto-merge / merge-when-pipeline-succeeds;
- poll the change's pipeline, merge, read the change's state.

`proto/forge/v1/forge.proto` defines the gRPC `ForgeService`; in-process
consumers use the hand-written async `Forge` trait whose methods take/return the
same proto messages (`RepoRef`, `ChangeRef`, `FileBlob`, `CiStatus`,
`ChangeState`). `GitHubForge` (octocrab + GraphQL auto-merge) and `GitLabForge`
(REST v4, Bearer auth, merge-when-pipeline-succeeds) implement it.

Built with [`wave`](https://github.com/fastverk/wave), the cross-repo
dependency-cascade engine.

## Install

`.bazelrc`:

```
common --registry=https://registry.fastverk.com/
common --registry=https://bcr.bazel.build/
```

`MODULE.bazel`:

```python
bazel_dep(name = "forge", version = "0.0.1")
```

## Testing

### Conformance suite

`tests/conformance.rs` is the executable spec for the `Forge` contract. Run it:

```sh
cargo test --features testing
```

All conformance cases run against `FakeForge` (an in-memory double) offline.
Real-adapter fixtures are replayed from JSON files in `tests/fixtures/` using
the `RecordedServer` harness in `src/recorded.rs`.

### Re-recording fixtures

When a real adapter needs re-recording (API change, new test cases):

```sh
# GitHub — needs a scratch repo and a token with repo + webhook scope
FORGE_RECORD=1 \
  GITHUB_TOKEN=ghp_... \
  FORGE_RECORD_OWNER=<scratch-org> \
  FORGE_RECORD_REPO=forge-conformance-scratch \
  cargo test --features testing -- github_recorded --nocapture

# GitLab — needs a scratch group/project and a token with api scope
FORGE_RECORD=1 \
  GITLAB_TOKEN=glpat-... \
  FORGE_RECORD_HOST=gitlab.com \
  FORGE_RECORD_OWNER=<scratch-group> \
  FORGE_RECORD_REPO=forge-conformance-scratch \
  cargo test --features testing -- gitlab_recorded --nocapture
```

The fixture file (`tests/fixtures/{github,gitlab}.json`) is written on exit.
Commit it; CI replays it with no credentials and no network access.

**Secrets are scrubbed automatically** — `Authorization` headers are never
written to fixture files.  See `tests/fixtures/README.md` and `src/recorded.rs`.

