//! A minimal HTTP mock server for wire-format tests.
//!
//! Deliberately hand-rolled on raw TCP instead of a framework: the whole
//! point is controlling exactly where response-body chunk boundaries fall
//! (e.g. cutting inside a multi-byte UTF-8 character inside an SSE event)
//! and being able to assert on the raw request bytes the client sent.
//! Responses are close-delimited (`Connection: close`, no Content-Length),
//! which reqwest handles fine.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// Pause between chunk writes so each surfaces as its own TCP segment.
/// Adjacent writes could otherwise coalesce in the client's read buffer,
/// silently weakening the chunk-boundary scenarios under test.
const INTER_CHUNK_DELAY: Duration = Duration::from_millis(10);

/// A request captured by [`HttpMockServer`].
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    /// Header names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl CapturedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn body_json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("request body should be valid JSON")
    }
}

/// One queued response. `chunks` are written one by one, each followed by a
/// flush and a small delay.
#[derive(Debug, Clone)]
pub struct MockResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub chunks: Vec<Vec<u8>>,
}

impl MockResponse {
    /// Single-chunk JSON response.
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "application/json",
            chunks: vec![body.into().into_bytes()],
        }
    }

    /// `text/event-stream` response whose body is split at the given byte
    /// offsets. Offsets must be strictly increasing and lie inside the body;
    /// splitting inside a multi-byte UTF-8 character is explicitly allowed
    /// and is one of the scenarios under test.
    pub fn sse_split(raw: &str, cut_offsets: &[usize]) -> Self {
        let bytes = raw.as_bytes();
        let mut chunks = Vec::new();
        let mut start = 0usize;
        for &off in cut_offsets {
            assert!(
                off > start && off < bytes.len(),
                "cut offsets must be strictly increasing and inside the body (got {off}, start {start}, len {})",
                bytes.len()
            );
            chunks.push(bytes[start..off].to_vec());
            start = off;
        }
        chunks.push(bytes[start..].to_vec());
        Self {
            status: 200,
            content_type: "text/event-stream",
            chunks,
        }
    }
}

struct SharedState {
    /// One response popped per incoming request.
    responses: Mutex<VecDeque<MockResponse>>,
    captured: Mutex<Vec<CapturedRequest>>,
}

/// A local HTTP server bound to `127.0.0.1:<random port>`. Dropping it stops
/// the accept loop; in-flight connections finish their current response.
pub struct HttpMockServer {
    port: u16,
    state: Arc<SharedState>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl HttpMockServer {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every request the server has handled, in arrival order.
    pub fn captured(&self) -> Vec<CapturedRequest> {
        self.state.captured.lock().unwrap().clone()
    }
}

impl Drop for HttpMockServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

pub async fn spawn_server(responses: Vec<MockResponse>) -> HttpMockServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(SharedState {
        responses: Mutex::new(responses.into()),
        captured: Mutex::new(Vec::new()),
    });

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let accept_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let state = accept_state.clone();
                    tokio::spawn(async move {
                        handle_connection(stream, state).await;
                    });
                }
            }
        }
    });

    HttpMockServer {
        port,
        state,
        shutdown: Some(shutdown_tx),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn handle_connection(mut stream: TcpStream, state: Arc<SharedState>) {
    // Read until the end of the request head.
    let mut buf: Vec<u8> = Vec::new();
    let mut scratch = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut scratch).await.unwrap_or(0);
        if n == 0 {
            return; // client vanished before sending a full head
        }
        buf.extend_from_slice(&scratch[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return; // runaway head; drop the connection
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body: Vec<u8> = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut scratch).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        body.extend_from_slice(&scratch[..n]);
    }
    body.truncate(content_length);

    state.captured.lock().unwrap().push(CapturedRequest {
        method,
        path,
        headers,
        body,
    });

    let response = state
        .responses
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| MockResponse::json(500, "{\"error\":\"no queued mock response\"}"));

    let reason = match response.status {
        200 => "OK",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "OK",
    };
    // Close-delimited body: no Content-Length, connection closed after the
    // last chunk. Keeps the writer dead simple and matches how SSE is served.
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
        response.status, reason, response.content_type
    );
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let _ = stream.flush().await;

    for chunk in &response.chunks {
        if stream.write_all(chunk).await.is_err() {
            return;
        }
        let _ = stream.flush().await;
        tokio::time::sleep(INTER_CHUNK_DELAY).await;
    }
    let _ = stream.shutdown().await;
}
