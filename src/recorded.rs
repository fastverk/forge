//! Recorded-fixture harness for offline conformance testing.
//!
//! # Overview
//!
//! The conformance suite in `tests/conformance.rs` is the executable spec for the
//! `Forge` contract. Today it only runs against `FakeForge`. The harness here adds
//! a second mode: a **real adapter** (GitHub, GitLab, …) whose HTTP traffic was
//! recorded against a scratch org and is replayed offline in CI.
//!
//! # Fixture format
//!
//! Each adapter gets one JSON file under `tests/fixtures/<adapter>.json`:
//!
//! ```json
//! {
//!   "adapter": "github",
//!   "exchanges": [
//!     {
//!       "method": "GET",
//!       "path": "/repos/acme/widgets",
//!       "status": 200,
//!       "response_headers": [["content-type", "application/json"]],
//!       "response_body": "{\"default_branch\":\"main\"}"
//!     }
//!   ]
//! }
//! ```
//!
//! Fixtures are pretty-printed so a re-record produces a readable, line-level diff.
//! **Authorization headers are never written**; see [`scrub_secrets`].
//!
//! # How to re-record
//!
//! ```sh
//! FORGE_RECORD=1 \
//!   GITHUB_TOKEN=ghp_... \
//!   FORGE_RECORD_REPO=acme/forge-conformance-scratch \
//!   cargo test --features testing -- github_recorded
//! ```
//!
//! The fixture file is written to `tests/fixtures/github.json` when the test
//! process exits. Commit the result; the file is the ground-truth replay used by CI.
//!
//! # How to use in a conformance fixture
//!
//! ```rust,ignore
//! use forge::recorded::RecordedServer;
//! use forge::github::GitHubForge;
//!
//! let server = RecordedServer::start(
//!     "tests/fixtures/github.json",
//!     "https://api.github.com",   // upstream — only used when FORGE_RECORD=1
//! ).await;
//!
//! let client = octocrab::Octocrab::builder()
//!     .base_uri(server.base_url())
//!     .unwrap()
//!     .build()
//!     .unwrap();
//! let forge = GitHubForge::from_client(client);
//! ```
//!
//! See `tests/conformance.rs` for the `Fixture` trait that this wires up to.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

// ── Exchange / FixtureFile ───────────────────────────────────────────────────

/// A single captured HTTP request/response exchange.
///
/// Only the data the adapter *reads* matters for replay: the response status,
/// headers, and body. Request details (method, path) are stored so a reviewer
/// can see what each response corresponds to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exchange {
    /// HTTP method (GET, POST, PUT, DELETE, PATCH, HEAD).
    pub method: String,

    /// Request path + query string (no scheme or host).
    ///
    /// Example: `"/repos/acme/widgets/git/refs/heads/main"`
    pub path: String,

    /// HTTP response status code.
    pub status: u16,

    /// Response headers as `(name, value)` pairs, lower-cased.
    ///
    /// `Authorization` headers are **never** written here; see [`scrub_secrets`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub response_headers: Vec<(String, String)>,

    /// Raw response body (UTF-8). JSON is stored verbatim for readability.
    pub response_body: String,
}

/// An ordered sequence of HTTP exchanges: one fixture file per adapter.
///
/// Serialized as pretty-printed JSON so a re-record produces a line-level diff.
/// Exchanges are consumed in the order they were recorded; the server returns a
/// 500 with a diagnostic message if a request arrives after all exchanges have
/// been consumed.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FixtureFile {
    /// Which adapter produced this recording ("github", "gitlab", …).
    #[serde(default)]
    pub adapter: String,

    /// The recorded exchanges, in the order they were made.
    pub exchanges: Vec<Exchange>,
}

impl FixtureFile {
    /// Load a fixture from `path`. Panics with a clear diagnostic on failure —
    /// a missing file means someone forgot to run the recorder.
    pub fn load(path: &Path) -> Self {
        let data = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("fixture file {} not found: {e}\n\nRun with FORGE_RECORD=1 to record it.", path.display()));
        serde_json::from_str(&data)
            .unwrap_or_else(|e| panic!("fixture file {} is corrupt: {e}", path.display()))
    }

    /// Persist to `path` as pretty-printed JSON.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }
}

// ── Secret scrubbing ─────────────────────────────────────────────────────────

/// Strip credentials from a header list before writing to a fixture file.
///
/// `Authorization` headers carry long-lived tokens on every request. Writing
/// one to git history is worse than no fixture at all. This function removes
/// any header whose name is `authorization` (case-insensitive), plus
/// `x-gitlab-token`, `x-hub-signature`, and `x-hub-signature-256` which carry
/// HMAC secrets in responses.
///
/// Call this on captured response headers before storing an [`Exchange`]; the
/// replay server never sees the real token anyway.
pub fn scrub_secrets(headers: &[(String, String)]) -> Vec<(String, String)> {
    const BLOCKED: &[&str] = &[
        "authorization",
        "x-gitlab-token",
        "x-hub-signature",
        "x-hub-signature-256",
        "x-oauth-scopes",
        "x-accepted-oauth-scopes",
    ];
    headers
        .iter()
        .filter(|(name, _)| {
            let lower = name.to_lowercase();
            !BLOCKED.contains(&lower.as_str())
        })
        .cloned()
        .collect()
}

// ── RecordedServer ───────────────────────────────────────────────────────────

/// A local HTTP server that replays recorded fixtures or — when `FORGE_RECORD`
/// is set — proxies to a real API and captures the traffic.
///
/// Configure the adapter's HTTP client to use [`RecordedServer::base_url`] as
/// its API base. In record mode the fixture file is written on [`Drop`].
pub struct RecordedServer {
    /// `http://127.0.0.1:<port>` — point the adapter's HTTP client here.
    base_url: String,

    /// Background listener task. Aborted when this struct is dropped (through
    /// the task handle being dropped).
    _task: tokio::task::JoinHandle<()>,

    /// Non-None in record mode; the captured exchanges are flushed to disk here.
    recorder: Option<Arc<Mutex<RecordState>>>,
}

struct RecordState {
    path: PathBuf,
    adapter: String,
    exchanges: Vec<Exchange>,
    /// Base URL of the real upstream API.
    upstream: String,
}

impl Drop for RecordedServer {
    fn drop(&mut self) {
        if let Some(rec) = self.recorder.take() {
            let guard = rec.lock().unwrap();
            let file = FixtureFile {
                adapter: guard.adapter.clone(),
                exchanges: guard.exchanges.clone(),
            };
            match file.save(&guard.path) {
                Ok(()) => eprintln!(
                    "forge-fixture: wrote {} exchanges to {:?}",
                    file.exchanges.len(),
                    guard.path
                ),
                Err(e) => eprintln!("forge-fixture: failed to save {:?}: {e}", guard.path),
            }
        }
    }
}

impl RecordedServer {
    /// Start the fixture server for `adapter` using `fixture_path`.
    ///
    /// **Replay mode** (default / CI): loads `fixture_path`, serves exchanges in
    /// order. Panics if the file is absent — run with `FORGE_RECORD=1` first.
    ///
    /// **Record mode** (`FORGE_RECORD=1`): proxies every request to `upstream`,
    /// captures the exchange (secrets scrubbed), writes `fixture_path` on drop.
    ///
    /// `upstream` is only consulted in record mode; it is ignored in replay.
    pub async fn start(
        fixture_path: impl AsRef<Path>,
        adapter: impl Into<String>,
        upstream: impl Into<String>,
    ) -> Self {
        let path = fixture_path.as_ref().to_path_buf();
        let adapter = adapter.into();
        let upstream = upstream.into();
        let record_mode = std::env::var("FORGE_RECORD").is_ok();

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let port = listener.local_addr().expect("local addr").port();
        let base_url = format!("http://127.0.0.1:{port}");

        if record_mode {
            let rec = Arc::new(Mutex::new(RecordState {
                path,
                adapter,
                exchanges: Vec::new(),
                upstream,
            }));
            let rec2 = Arc::clone(&rec);
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let rec = Arc::clone(&rec2);
                    tokio::spawn(async move {
                        if let Err(e) = handle_record(stream, rec).await {
                            eprintln!("forge-fixture record: {e}");
                        }
                    });
                }
            });
            Self {
                base_url,
                _task: task,
                recorder: Some(rec),
            }
        } else {
            let file = FixtureFile::load(&path);
            let queue: Arc<Mutex<VecDeque<Exchange>>> =
                Arc::new(Mutex::new(file.exchanges.into_iter().collect()));
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let q = Arc::clone(&queue);
                    tokio::spawn(async move {
                        if let Err(e) = handle_replay(stream, q).await {
                            eprintln!("forge-fixture replay: {e}");
                        }
                    });
                }
            });
            Self {
                base_url,
                _task: task,
                recorder: None,
            }
        }
    }

    /// `http://127.0.0.1:<port>` — configure the adapter's HTTP client with this
    /// as its API base URL so all requests go through the fixture server.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

// ── Minimal HTTP/1.1 server ──────────────────────────────────────────────────
//
// The adapters (octocrab / reqwest) speak HTTP/1.1 to any plain-http URL.
// We implement just enough of the protocol to serve them:
//   – request-line + headers + body parsing
//   – response-line + headers + body writing
//   – keep-alive (multiple requests per connection) via the read loop
//
// "Connection: close" is added to every response so the adapter releases the
// TCP connection after reading the response body, which keeps the loop simple.

/// Parse one HTTP/1.1 request from `reader`.
///
/// Returns `None` on clean EOF (connection closed), or the parsed
/// `(method, path, headers, body)` tuple.
async fn read_request(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> std::io::Result<Option<(String, String, Vec<(String, String)>, Vec<u8>)>> {
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => return Ok(None), // EOF
        Ok(_) => {}
        Err(e) => return Err(e),
    }
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return Ok(None);
    }

    let mut parts = trimmed.splitn(3, ' ');
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut content_length: usize = 0;

    loop {
        let mut hline = String::new();
        if reader.read_line(&mut hline).await? == 0 {
            break;
        }
        let hline = hline.trim_end_matches(['\r', '\n']);
        if hline.is_empty() {
            break; // blank line = end of headers
        }
        if let Some((k, v)) = hline.split_once(':') {
            let k = k.trim().to_lowercase();
            let v = v.trim().to_string();
            if k == "content-length" {
                content_length = v.parse().unwrap_or_else(|_| {
                    tracing::warn!("forge-fixture: invalid content-length {:?}, defaulting to 0", v);
                    0
                });
            }
            headers.push((k, v));
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await?;
    }

    Ok(Some((method, path, headers, body)))
}

/// Write a complete HTTP/1.1 response (status + headers + body).
async fn write_response(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    headers: &[(String, String)],
    body: &str,
) -> std::io::Result<()> {
    let reason = reason_phrase(status);
    let mut buf = format!("HTTP/1.1 {status} {reason}\r\n");

    for (k, v) in headers {
        buf.push_str(k);
        buf.push_str(": ");
        buf.push_str(v);
        buf.push_str("\r\n");
    }
    buf.push_str(&format!("content-length: {}\r\n", body.len()));
    // Tell the client to close after this response so the read loop exits cleanly.
    buf.push_str("connection: close\r\n\r\n");
    buf.push_str(body);

    writer.write_all(buf.as_bytes()).await
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

// ── Replay handler ───────────────────────────────────────────────────────────

async fn handle_replay(
    stream: TcpStream,
    queue: Arc<Mutex<VecDeque<Exchange>>>,
) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);

    while let Some((method, path, _req_hdrs, _body)) = read_request(&mut reader).await? {
        let ex = queue.lock().unwrap().pop_front();
        match ex {
            Some(ex) => {
                write_response(&mut w, ex.status, &ex.response_headers, &ex.response_body).await?;
            }
            None => {
                let msg = format!(
                    "{{\"error\":\"forge-fixture: no recorded exchange for {method} {path}\"}}",
                );
                eprintln!("forge-fixture: queue exhausted on {method} {path}");
                write_response(&mut w, 500, &[], &msg).await?;
            }
        }
    }
    Ok(())
}

// ── Record / proxy handler ───────────────────────────────────────────────────

async fn handle_record(
    stream: TcpStream,
    rec: Arc<Mutex<RecordState>>,
) -> std::io::Result<()> {
    let upstream = rec.lock().unwrap().upstream.clone();

    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);

    // One reqwest::Client per connection is fine for testing.
    let client = reqwest::Client::builder()
        .user_agent("forge-fixture-recorder")
        .build()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    while let Some((method, path, req_hdrs, body)) = read_request(&mut reader).await? {
        let url = format!("{upstream}{path}");

        let method_parsed = reqwest::Method::from_bytes(method.as_bytes())
            .unwrap_or(reqwest::Method::GET);
        let mut builder = client.request(method_parsed, &url);

        // Forward all headers except hop-by-hop ones (Host is set by reqwest).
        for (k, v) in &req_hdrs {
            match k.as_str() {
                "host" | "content-length" | "transfer-encoding" | "connection" => {}
                _ => {
                    builder = builder.header(k.as_str(), v.as_str());
                }
            }
        }
        if !body.is_empty() {
            builder = builder.body(body);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let status = resp.status().as_u16();
        let resp_headers_raw: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_lowercase(), s.to_string())))
            .collect();
        // Scrub secrets BEFORE storing — never write tokens to fixture files.
        let resp_headers = scrub_secrets(&resp_headers_raw);

        let resp_body = resp
            .text()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let exchange = Exchange {
            method: method.clone(),
            path: path.clone(),
            status,
            response_headers: resp_headers.clone(),
            response_body: resp_body.clone(),
        };

        rec.lock().unwrap().exchanges.push(exchange);

        write_response(&mut w, status, &resp_headers, &resp_body).await?;
    }
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_secrets_removes_authorization() {
        let headers = vec![
            ("authorization".to_string(), "******".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
            ("x-hub-signature-256".to_string(), "sha256=abc".to_string()),
        ];
        let scrubbed = scrub_secrets(&headers);
        assert_eq!(scrubbed.len(), 1);
        assert_eq!(scrubbed[0].0, "content-type");
    }

    #[test]
    fn scrub_secrets_case_insensitive() {
        let headers = vec![
            ("Authorization".to_string(), "token abc".to_string()),
            ("AUTHORIZATION".to_string(), "******".to_string()),
            ("x-request-id".to_string(), "123".to_string()),
        ];
        let scrubbed = scrub_secrets(&headers);
        assert_eq!(scrubbed.len(), 1);
        assert_eq!(scrubbed[0].0, "x-request-id");
    }

    #[test]
    fn fixture_file_roundtrip() {
        let file = FixtureFile {
            adapter: "github".to_string(),
            exchanges: vec![Exchange {
                method: "GET".to_string(),
                path: "/repos/acme/widgets".to_string(),
                status: 200,
                response_headers: vec![("content-type".to_string(), "application/json".to_string())],
                response_body: r#"{"default_branch":"main"}"#.to_string(),
            }],
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        file.save(&path).unwrap();

        let loaded = FixtureFile::load(&path);
        assert_eq!(loaded.adapter, "github");
        assert_eq!(loaded.exchanges.len(), 1);
        assert_eq!(loaded.exchanges[0].method, "GET");
        assert_eq!(loaded.exchanges[0].status, 200);
    }

    #[tokio::test]
    async fn replay_server_serves_exchange() {
        // Write a one-exchange fixture to a temp file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        FixtureFile {
            adapter: "test".to_string(),
            exchanges: vec![Exchange {
                method: "GET".to_string(),
                path: "/hello".to_string(),
                status: 200,
                response_headers: vec![("content-type".to_string(), "application/json".to_string())],
                response_body: r#"{"ok":true}"#.to_string(),
            }],
        }
        .save(&path)
        .unwrap();

        // Ensure we are NOT in record mode.
        std::env::remove_var("FORGE_RECORD");

        let server = RecordedServer::start(&path, "test", "http://unused").await;

        let resp = reqwest::get(format!("{}/hello", server.base_url()))
            .await
            .expect("request to fixture server");
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ok"], true);
    }

    #[tokio::test]
    async fn replay_server_returns_500_when_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.json");
        FixtureFile {
            adapter: "test".to_string(),
            exchanges: vec![],
        }
        .save(&path)
        .unwrap();

        std::env::remove_var("FORGE_RECORD");

        let server = RecordedServer::start(&path, "test", "http://unused").await;

        let resp = reqwest::get(format!("{}/anything", server.base_url()))
            .await
            .expect("request to fixture server");
        assert_eq!(resp.status(), 500);
    }
}
