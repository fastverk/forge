# tests/fixtures/

Recorded HTTP fixtures for the conformance suite.

Each file is a JSON array of request/response exchanges captured against a
real forge API (GitHub, GitLab) and replayed offline in CI.  The files are
pretty-printed so re-recordings produce readable diffs.

## File format

```json
{
  "adapter": "github",
  "exchanges": [
    {
      "method": "GET",
      "path": "/repos/acme/forge-conformance-scratch",
      "status": 200,
      "response_headers": [["content-type", "application/json"]],
      "response_body": "{\"default_branch\":\"main\",...}"
    }
  ]
}
```

`Authorization` headers are **never written** — `scrub_secrets` strips them
before the exchange is stored.  See `src/recorded.rs` for the full spec.

## How to re-record

### GitHub

```sh
FORGE_RECORD=1 \
  GITHUB_TOKEN=ghp_... \
  FORGE_RECORD_OWNER=<scratch-org> \
  FORGE_RECORD_REPO=forge-conformance-scratch \
  cargo test --features testing -- github_recorded --nocapture
```

The fixture is written to `tests/fixtures/github.json` when the test exits.

### GitLab

```sh
FORGE_RECORD=1 \
  GITLAB_TOKEN=glpat-... \
  FORGE_RECORD_HOST=gitlab.com \
  FORGE_RECORD_OWNER=<scratch-group> \
  FORGE_RECORD_REPO=forge-conformance-scratch \
  cargo test --features testing -- gitlab_recorded --nocapture
```

The fixture is written to `tests/fixtures/gitlab.json`.

## CI

CI runs `cargo test --features testing` with no credentials and no network
access.  The replay server (`RecordedServer`) binds a local port, serves the
checked-in exchanges in order, and tears down when the test exits.

If a fixture file is missing the test panics with a clear "run with
FORGE_RECORD=1" message.  If the fixture is present but exhausted before the
test finishes the server returns HTTP 500 and the conformance assertion fails
with a diagnostic.
