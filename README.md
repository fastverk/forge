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

Then depend on `@forge//:forge` from a `rust_library`. For Cargo/IDE iteration the
crate also builds standalone (`cargo build`).
