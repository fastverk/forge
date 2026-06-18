# Changelog

All notable changes to forge. The format is loosely
[Keep a Changelog](https://keepachangelog.com/) — version headers mirror the
published bazel-registry entries.

## 0.0.1 — initial

- `proto/forge/v1/forge.proto`: generic gRPC `ForgeService` + message DTOs
  (`RepoRef`, `ChangeRef`, `FileBlob`, `CiStatus`, `ChangeState`).
- Async `Forge` trait + `GitHubForge` (octocrab + GraphQL auto-merge) and
  `GitLabForge` (REST v4, Bearer auth, merge-when-pipeline-succeeds) adapters.
