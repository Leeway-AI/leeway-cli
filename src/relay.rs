//! Subscription-mode localhost relay.
//!
//! Claude Code is pointed at http://127.0.0.1:<port>; its OAuth credential
//! flows HERE and only here. For POST /v1/messages the relay:
//!   1. strips and HOLDS Authorization / x-api-key / all anthropic-* headers
//!      — these NEVER go to the Leeway gateway;
//!   2. posts the BODY to <gateway>/anthropic/v1/optimize (lwllm key only);
//!   3. sends the optimized body to Anthropic with the held credential, from
//!      this machine, and streams the response back unbuffered (tee for usage);
//!   4. fail-open: any gateway trouble (timeout/5xx/429/402) forwards the
//!      ORIGINAL body to Anthropic — a session is never blocked by Leeway;
//!   5. async post-response: reports actuals (+ a locally-counted baseline via
//!      the FREE count_tokens endpoint) back to the gateway. Best-effort.
//!
//! Every other path (count_tokens included) is a direct local passthrough to
//! Anthropic with the held headers — the gateway is not involved.

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use axum::Router;
use bytes::Bytes;
use futures_util::Stream;
use serde::Deserialize;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

pub const DEFAULT_ANTHROPIC_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_OPTIMIZE_TIMEOUT_MS: u64 = 10_000;
/// how much of a response we keep for usage extraction (the client stream is
/// never limited — this caps only the side buffer)
const TEE_CAP_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub gateway_url: String,
    pub api_key: String,
    pub mode: String,
    pub session_id: String,
    /// stable anonymous device token (plan seat accounting); empty = unknown
    pub device_id: String,
    pub cli_version: String,
    pub anthropic_url: String,
    pub optimize_timeout: Duration,
    /// suppress stderr notices (tests)
    pub quiet: bool,
}

impl RelayConfig {
    pub fn new(
        gateway_url: String,
        api_key: String,
        mode: String,
        session_id: String,
        device_id: String,
    ) -> Self {
        let anthropic_url = std::env::var("LEEWAY_ANTHROPIC_URL")
            .unwrap_or_else(|_| DEFAULT_ANTHROPIC_URL.to_string());
        let optimize_timeout = Duration::from_millis(
            std::env::var("LEEWAY_OPTIMIZE_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_OPTIMIZE_TIMEOUT_MS),
        );
        RelayConfig {
            gateway_url,
            api_key,
            mode,
            session_id,
            device_id,
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            anthropic_url,
            optimize_timeout,
            quiet: false,
        }
    }
}

#[derive(Default)]
pub struct RelayStats {
    pub requests: AtomicU64,
    pub optimized: AtomicU64,
    /// optimizations skipped (gateway down/slow/limited/402) — the session continued
    pub fail_open: AtomicU64,
    /// a 402 switched the session to pure passthrough
    pub plan_blocked: AtomicBool,
    upgrade_hint_shown: AtomicBool,
    limit_hint_shown: AtomicBool,
}

struct RelayState {
    cfg: RelayConfig,
    /// keep-alive pool to Anthropic
    anthropic: reqwest::Client,
    gateway: reqwest::Client,
    stats: Arc<RelayStats>,
    /// in-flight report-back tasks — shutdown drains them (bounded wait)
    pending_reports: Arc<AtomicU64>,
}

/// decrements on drop, so cancelled report tasks never wedge the shutdown drain
struct PendingGuard(Arc<AtomicU64>);
impl PendingGuard {
    fn new(counter: &Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        PendingGuard(counter.clone())
    }
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct RelayHandle {
    pub addr: SocketAddr,
    pub stats: Arc<RelayStats>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
    pending_reports: Arc<AtomicU64>,
}

impl RelayHandle {
    /// Graceful stop — called when the child exits. Drains in-flight actuals
    /// reports (bounded) so the last request's receipt is not lost.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.task.await;
        for _ in 0..120 {
            if self.pending_reports.load(Ordering::SeqCst) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

pub async fn start_relay(cfg: RelayConfig) -> Result<RelayHandle> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("binding the localhost relay")?;
    let addr = listener.local_addr()?;
    let stats = Arc::new(RelayStats::default());
    let mk_client = || {
        reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .user_agent(format!("leeway-cli/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building http client")
    };
    let pending_reports = Arc::new(AtomicU64::new(0));
    let state = Arc::new(RelayState {
        anthropic: mk_client()?,
        gateway: mk_client()?,
        stats: stats.clone(),
        pending_reports: pending_reports.clone(),
        cfg,
    });
    let app = Router::new().fallback(handler).with_state(state);
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
    });
    Ok(RelayHandle {
        addr,
        stats,
        shutdown: Some(tx),
        task,
        pending_reports,
    })
}

// ---------------------------------------------------------------------------
// request handling
// ---------------------------------------------------------------------------

/// headers we never forward in either direction (hop-by-hop / recomputed)
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host" | "content-length" | "transfer-encoding" | "connection" | "keep-alive" | "expect"
            // identity encoding keeps the tee'd usage parser readable; the
            // upstream response is re-encoded transparently by reqwest anyway
            | "accept-encoding"
    )
}

fn local_origin(headers: &HeaderMap) -> bool {
    match headers.get("origin").and_then(|v| v.to_str().ok()) {
        None => true,
        Some(o) => o.starts_with("http://127.0.0.1") || o.starts_with("http://localhost"),
    }
}

async fn handler(State(st): State<Arc<RelayState>>, req: Request<Body>) -> Response<Body> {
    if !local_origin(req.headers()) {
        return plain_response(StatusCode::FORBIDDEN, "leeway relay: local clients only");
    }
    st.stats.requests.fetch_add(1, Ordering::Relaxed);

    let method = req.method().clone();
    let path_q = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();
    let body = match axum::body::to_bytes(req.into_body(), 256 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return plain_response(StatusCode::BAD_REQUEST, "leeway relay: unreadable body"),
    };

    if method == Method::POST && path == "/v1/messages" {
        messages_flow(st, headers, body).await
    } else {
        // count_tokens and any other path: direct local passthrough, held headers
        forward_to_anthropic(st, method, &path_q, &headers, body, None, None).await
    }
}

#[derive(Deserialize)]
struct OptimizeResponse {
    #[serde(rename = "auditId")]
    audit_id: String,
    #[serde(rename = "optimizedBody")]
    optimized_body: serde_json::Value,
    #[allow(dead_code)]
    mode: String,
    passthrough: bool,
}

enum OptimizeOutcome {
    Optimized(OptimizeResponse),
    PlanRequired(String),
    /// 429 — device seat limit or rate limit; temporary, NOT a plan block
    Limited(String),
    Unavailable,
}

async fn call_optimize(st: &RelayState, original: &serde_json::Value) -> OptimizeOutcome {
    let url = format!(
        "{}/anthropic/v1/optimize",
        st.cfg.gateway_url.trim_end_matches('/')
    );
    let mut meta = serde_json::json!({
        "mode": st.cfg.mode, "sessionId": st.cfg.session_id, "cliVersion": st.cfg.cli_version,
    });
    if !st.cfg.device_id.is_empty() {
        meta["deviceId"] = serde_json::Value::String(st.cfg.device_id.clone());
    }
    let payload = serde_json::json!({ "request": original, "meta": meta });
    let res = st
        .gateway
        .post(url)
        .header("x-leeway-api-key", &st.cfg.api_key)
        .timeout(st.cfg.optimize_timeout)
        .json(&payload)
        .send()
        .await;
    let res = match res {
        Ok(r) => r,
        Err(_) => return OptimizeOutcome::Unavailable,
    };
    if res.status() == StatusCode::PAYMENT_REQUIRED {
        let message = res
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| {
                v.pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| "Gateway access requires a paid Leeway plan.".to_string());
        return OptimizeOutcome::PlanRequired(message);
    }
    if res.status() == StatusCode::TOO_MANY_REQUESTS {
        let message = res
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| {
                v.pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| "Gateway rate limit reached.".to_string());
        return OptimizeOutcome::Limited(message);
    }
    if !res.status().is_success() {
        return OptimizeOutcome::Unavailable;
    }
    match res.json::<OptimizeResponse>().await {
        Ok(opt) => OptimizeOutcome::Optimized(opt),
        Err(_) => OptimizeOutcome::Unavailable,
    }
}

async fn messages_flow(st: Arc<RelayState>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    // unparsable body: nothing to optimize — straight to Anthropic
    let original: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return forward_to_anthropic(
                st,
                Method::POST,
                "/v1/messages",
                &headers,
                body,
                None,
                None,
            )
            .await
        }
    };

    let mut audit_id: Option<String> = None;
    let mut out_body = body.clone();

    if st.stats.plan_blocked.load(Ordering::Relaxed) {
        // a 402 put this session in pure passthrough — don't ping the gateway again
        st.stats.fail_open.fetch_add(1, Ordering::Relaxed);
    } else {
        match call_optimize(&st, &original).await {
            OptimizeOutcome::Optimized(opt) => {
                // off mode and passthrough answers keep the ORIGINAL bytes —
                // byte-faithful beats a re-serialization of the same JSON
                if !opt.passthrough && st.cfg.mode != "off" {
                    if let Ok(bytes) = serde_json::to_vec(&opt.optimized_body) {
                        out_body = Bytes::from(bytes);
                        st.stats.optimized.fetch_add(1, Ordering::Relaxed);
                    }
                }
                audit_id = Some(opt.audit_id);
            }
            OptimizeOutcome::PlanRequired(message) => {
                st.stats.plan_blocked.store(true, Ordering::Relaxed);
                st.stats.fail_open.fetch_add(1, Ordering::Relaxed);
                if !st.cfg.quiet && !st.stats.upgrade_hint_shown.swap(true, Ordering::Relaxed) {
                    eprintln!("\n[leeway] {message}\n[leeway] continuing WITHOUT optimization (pure passthrough) for this session.\n");
                }
            }
            OptimizeOutcome::Limited(message) => {
                // temporary (seat/rate limit) — keep trying, the session never blocks
                st.stats.fail_open.fetch_add(1, Ordering::Relaxed);
                if !st.cfg.quiet && !st.stats.limit_hint_shown.swap(true, Ordering::Relaxed) {
                    eprintln!("\n[leeway] {message}\n[leeway] requests continue unoptimized until a seat/limit frees up.\n");
                }
            }
            OptimizeOutcome::Unavailable => {
                // fail-open is mandatory: the user's session never blocks on Leeway
                st.stats.fail_open.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let baseline_ctx = audit_id.as_ref().map(|_| original);
    forward_to_anthropic(
        st,
        Method::POST,
        "/v1/messages",
        &headers,
        out_body,
        audit_id,
        baseline_ctx,
    )
    .await
}

/// Sends to Anthropic with the HELD credential + anthropic-* headers and
/// streams the response back unbuffered. When `audit_id` is set, the response
/// is tee'd and actuals (+ a locally counted baseline on `original`) are
/// reported to the gateway after the stream completes — never blocking it.
async fn forward_to_anthropic(
    st: Arc<RelayState>,
    method: Method,
    path_q: &str,
    headers: &HeaderMap,
    body: Bytes,
    audit_id: Option<String>,
    original: Option<serde_json::Value>,
) -> Response<Body> {
    let url = format!("{}{}", st.cfg.anthropic_url.trim_end_matches('/'), path_q);
    let mut rb = st.anthropic.request(method.clone(), url);
    for (name, value) in headers {
        if is_hop_by_hop(name) {
            continue;
        }
        rb = rb.header(name, value);
    }
    let started = Instant::now();
    let upstream = match rb.body(body).send().await {
        Ok(r) => r,
        Err(err) => {
            if let Some(id) = audit_id {
                report_actuals(st.clone(), id, None, 0, started.elapsed(), None).await_detached();
            }
            return plain_response(
                StatusCode::BAD_GATEWAY,
                &format!("leeway relay: could not reach Anthropic: {err}"),
            );
        }
    };

    let status = upstream.status();
    let mut response = Response::builder().status(status);
    if let Some(h) = response.headers_mut() {
        for (name, value) in upstream.headers() {
            if is_hop_by_hop(name) {
                continue;
            }
            h.insert(name.clone(), value.clone());
        }
    }

    let stream = upstream.bytes_stream();
    let body = if let Some(id) = audit_id {
        let (tee, done) = Tee::new(stream, TEE_CAP_BYTES);
        // held credential headers for the local FREE count call
        let count_headers = credential_headers(headers);
        let st2 = st.clone();
        let guard = PendingGuard::new(&st.pending_reports);
        tokio::spawn(async move {
            let _guard = guard;
            let collected = match done.await {
                Ok(bytes) => bytes,
                Err(_) => return,
            };
            let latency = started.elapsed();
            let usage = parse_usage(&collected);
            let baseline = match &original {
                Some(orig) => count_baseline(&st2, orig, &count_headers).await,
                None => None,
            };
            report_actuals(st2, id, usage, status.as_u16(), latency, baseline).await;
        });
        Body::from_stream(tee)
    } else {
        Body::from_stream(stream)
    };
    response.body(body).unwrap_or_else(|_| {
        plain_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "leeway relay: response build failed",
        )
    })
}

fn plain_response(status: StatusCode, message: &str) -> Response<Body> {
    let body = serde_json::json!({ "type": "error", "error": { "type": "api_error", "message": message } });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("static response")
}

/// the held credential + anthropic-* headers (for the local count_tokens call)
fn credential_headers(headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    headers
        .iter()
        .filter(|(name, _)| {
            let n = name.as_str();
            n == "authorization" || n == "x-api-key" || n.starts_with("anthropic-")
        })
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// tee + usage extraction + report-back
// ---------------------------------------------------------------------------

/// Forwards the inner stream untouched while keeping a capped copy; resolves
/// the oneshot when the stream ends OR is dropped (client disconnect) so the
/// report-back always runs with whatever was observed.
struct Tee<S> {
    inner: S,
    buf: Vec<u8>,
    cap: usize,
    done: Option<tokio::sync::oneshot::Sender<Vec<u8>>>,
}

impl<S> Tee<S> {
    fn new(inner: S, cap: usize) -> (Self, tokio::sync::oneshot::Receiver<Vec<u8>>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            Tee {
                inner,
                buf: Vec::new(),
                cap,
                done: Some(tx),
            },
            rx,
        )
    }
    fn finish(&mut self) {
        if let Some(tx) = self.done.take() {
            let _ = tx.send(std::mem::take(&mut self.buf));
        }
    }
}

impl<S> Stream for Tee<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if self.buf.len() < self.cap {
                    let room = self.cap - self.buf.len();
                    let take = room.min(chunk.len());
                    let slice = chunk.slice(0..take);
                    self.buf.extend_from_slice(&slice);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                self.finish();
                Poll::Ready(Some(Err(std::io::Error::other(err))))
            }
            Poll::Ready(None) => {
                self.finish();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> Drop for Tee<S> {
    fn drop(&mut self) {
        self.finish();
    }
}

#[derive(Debug, PartialEq, serde::Serialize)]
pub struct RawUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
}

/// Raw Anthropic usage from either a JSON response or an SSE event stream
/// (message_start carries input/cache counts, message_delta the output count).
pub fn parse_usage(bytes: &[u8]) -> Option<RawUsage> {
    let text = std::str::from_utf8(bytes).ok()?;
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        return usage_from_value(v.get("usage")?);
    }
    let mut found: Option<RawUsage> = None;
    for line in text.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload.trim()) else {
            continue;
        };
        let usage = match v.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => v.pointer("/message/usage"),
            Some("message_delta") => v.get("usage"),
            _ => None,
        };
        let Some(u) = usage else { continue };
        let entry = found.get_or_insert(RawUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        });
        if let Some(n) = u.get("input_tokens").and_then(|n| n.as_u64()) {
            entry.input_tokens = n;
        }
        if let Some(n) = u.get("output_tokens").and_then(|n| n.as_u64()) {
            entry.output_tokens = n;
        }
        if let Some(n) = u.get("cache_read_input_tokens").and_then(|n| n.as_u64()) {
            entry.cache_read_input_tokens = Some(n);
        }
        if let Some(n) = u
            .get("cache_creation_input_tokens")
            .and_then(|n| n.as_u64())
        {
            entry.cache_creation_input_tokens = Some(n);
        }
    }
    found
}

fn usage_from_value(u: &serde_json::Value) -> Option<RawUsage> {
    Some(RawUsage {
        input_tokens: u.get("input_tokens").and_then(|n| n.as_u64())?,
        output_tokens: u.get("output_tokens").and_then(|n| n.as_u64()).unwrap_or(0),
        cache_read_input_tokens: u.get("cache_read_input_tokens").and_then(|n| n.as_u64()),
        cache_creation_input_tokens: u
            .get("cache_creation_input_tokens")
            .and_then(|n| n.as_u64()),
    })
}

/// FREE local count of the ORIGINAL body (generation params stripped) with the
/// user's own credential. Best-effort.
async fn count_baseline(
    st: &RelayState,
    original: &serde_json::Value,
    held: &[(HeaderName, HeaderValue)],
) -> Option<u64> {
    let mut body = serde_json::Map::new();
    for key in ["model", "messages", "system", "tools", "tool_choice"] {
        if let Some(v) = original.get(key) {
            body.insert(key.to_string(), v.clone());
        }
    }
    let url = format!(
        "{}/v1/messages/count_tokens",
        st.cfg.anthropic_url.trim_end_matches('/')
    );
    let mut rb = st
        .anthropic
        .post(url)
        .json(&serde_json::Value::Object(body));
    for (name, value) in held {
        rb = rb.header(name, value);
    }
    let res = rb.timeout(Duration::from_secs(10)).send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    let v: serde_json::Value = res.json().await.ok()?;
    v.get("input_tokens").and_then(|n| n.as_u64())
}

async fn report_actuals(
    st: Arc<RelayState>,
    audit_id: String,
    usage: Option<RawUsage>,
    provider_status: u16,
    latency: Duration,
    baseline: Option<u64>,
) {
    let url = format!(
        "{}/anthropic/v1/optimize/{}/actuals",
        st.cfg.gateway_url.trim_end_matches('/'),
        audit_id
    );
    let mut payload = serde_json::json!({
        "providerStatus": provider_status,
        "latencyMs": latency.as_millis() as u64,
    });
    if let Some(u) = usage {
        payload["usage"] = serde_json::to_value(u).unwrap_or(serde_json::Value::Null);
    }
    if let Some(b) = baseline {
        payload["baselineTokens"] = serde_json::json!(b);
        payload["baselineSource"] = serde_json::json!("provider_count");
    }
    // best-effort, silent on failure — never blocks or breaks the session
    let _ = st
        .gateway
        .post(url)
        .header("x-leeway-api-key", &st.cfg.api_key)
        .timeout(Duration::from_secs(5))
        .json(&payload)
        .send()
        .await;
}

/// fire-and-forget wrapper used on the early-error path
trait FireAndForget {
    fn await_detached(self);
}
impl<F: std::future::Future<Output = ()> + Send + 'static> FireAndForget for F {
    fn await_detached(self) {
        tokio::spawn(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_usage() {
        let body = br#"{"id":"msg_1","usage":{"input_tokens":120,"output_tokens":40,"cache_read_input_tokens":80}}"#;
        let u = parse_usage(body).unwrap();
        assert_eq!(u.input_tokens, 120);
        assert_eq!(u.output_tokens, 40);
        assert_eq!(u.cache_read_input_tokens, Some(80));
        assert_eq!(u.cache_creation_input_tokens, None);
    }

    #[test]
    fn parses_sse_usage() {
        let sse = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":25,"cache_creation_input_tokens":10}}}"#,
            "\n\n",
            "event: content_block_delta\n",
            r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#,
            "\n\n",
            "event: message_delta\n",
            r#"data: {"type":"message_delta","usage":{"output_tokens":55}}"#,
            "\n\n",
        );
        let u = parse_usage(sse.as_bytes()).unwrap();
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 55);
        assert_eq!(u.cache_read_input_tokens, Some(25));
        assert_eq!(u.cache_creation_input_tokens, Some(10));
    }

    #[test]
    fn no_usage_means_none() {
        assert!(parse_usage(b"not json at all").is_none());
        assert!(parse_usage(br#"{"id":"x"}"#).is_none());
    }
}
