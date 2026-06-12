//! Relay integration: a mock Anthropic server + a mock Leeway gateway, with
//! the real relay in between. The make-or-break assertion: NO Anthropic
//! credential (Authorization, x-api-key, anthropic-*) ever reaches the
//! gateway, while Anthropic receives the held credential + the optimized body.

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use leeway_cli::relay::{start_relay, RelayConfig, RelayHandle};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SSE_FIXTURE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":25,\"cache_creation_input_tokens\":10}}}\n",
    "\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n",
    "\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":55}}\n",
    "\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n",
    "\n",
);

#[derive(Clone)]
struct Recorded {
    calls: Arc<Mutex<Vec<Call>>>,
}

#[derive(Clone, Debug)]
struct Call {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Recorded {
    fn new() -> Self {
        Recorded {
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn snapshot(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

async fn record(rec: &Recorded, req: Request<Body>) -> Call {
    let path = req.uri().path().to_string();
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap_or_default()
        .to_vec();
    let call = Call {
        path,
        headers,
        body,
    };
    rec.calls.lock().unwrap().push(call.clone());
    call
}

async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    addr
}

/// mock api.anthropic.com: /v1/messages (JSON or SSE), count_tokens, anything else JSON
async fn mock_anthropic(rec: Recorded) -> String {
    let app = Router::new().fallback(move |req: Request<Body>| {
        let rec = rec.clone();
        async move {
            let call = record(&rec, req).await;
            if call.path == "/v1/messages/count_tokens" {
                return json_response(StatusCode::OK, r#"{"input_tokens":1234}"#);
            }
            if call.path == "/v1/messages" {
                let streaming = serde_json::from_slice::<serde_json::Value>(&call.body)
                    .ok()
                    .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
                    .unwrap_or(false);
                if streaming {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(SSE_FIXTURE))
                        .unwrap();
                }
                return json_response(
                    StatusCode::OK,
                    r#"{"id":"msg_1","type":"message","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":120,"output_tokens":40,"cache_read_input_tokens":80}}"#,
                );
            }
            json_response(StatusCode::OK, r#"{"ok":true,"path":"other"}"#)
        }
    });
    format!("http://{}", serve(app).await)
}

enum GatewayKind {
    Optimizing,
    PlanRequired,
}

/// mock leeway gateway: optimize rewrites messages[0].content to "OPTIMIZED"
async fn mock_gateway(rec: Recorded, kind: GatewayKind) -> String {
    let kind = Arc::new(kind);
    let app = Router::new().fallback(move |req: Request<Body>| {
        let rec = rec.clone();
        let kind = kind.clone();
        async move {
            let call = record(&rec, req).await;
            if call.path == "/anthropic/v1/optimize" {
                return match *kind {
                    GatewayKind::PlanRequired => json_response(
                        StatusCode::PAYMENT_REQUIRED,
                        r#"{"type":"error","error":{"type":"plan_required","message":"Gateway access requires a Pro plan or higher. Upgrade at /app/billing"}}"#,
                    ),
                    GatewayKind::Optimizing => {
                        let envelope: serde_json::Value = serde_json::from_slice(&call.body).unwrap();
                        let mut optimized = envelope.get("request").cloned().unwrap_or_default();
                        if let Some(m) = optimized.pointer_mut("/messages/0/content") {
                            *m = serde_json::json!("OPTIMIZED");
                        }
                        let body = serde_json::json!({
                            "auditId": "req_TEST1",
                            "optimizedBody": optimized,
                            "mode": "safe",
                            "passthrough": false,
                        });
                        json_response(StatusCode::OK, &body.to_string())
                    }
                };
            }
            if call.path.starts_with("/anthropic/v1/optimize/") && call.path.ends_with("/actuals") {
                return json_response(StatusCode::OK, r#"{"ok":true}"#);
            }
            json_response(StatusCode::NOT_FOUND, r#"{"error":{"message":"mock gateway: unknown path"}}"#)
        }
    });
    format!("http://{}", serve(app).await)
}

fn json_response(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn relay_config(gateway_url: String, anthropic_url: String) -> RelayConfig {
    RelayConfig {
        gateway_url,
        api_key: "lwllm_relay_test_key".to_string(),
        mode: "safe".to_string(),
        session_id: "relay-it-session".to_string(),
        device_id: "dev-relay-it".to_string(),
        cli_version: "test".to_string(),
        anthropic_url,
        optimize_timeout: Duration::from_secs(2),
        quiet: true,
    }
}

async fn start(gateway_url: String, anthropic_url: String) -> RelayHandle {
    start_relay(relay_config(gateway_url, anthropic_url))
        .await
        .expect("relay starts")
}

fn original_body() -> serde_json::Value {
    serde_json::json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 1024,
        "messages": [{ "role": "user", "content": "the original prompt" }],
        "metadata": { "user_id": "u1" },
    })
}

async fn wait_for<F: Fn() -> bool>(what: &str, cond: F) {
    for _ in 0..100 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

const OAUTH_CANARY: &str = "sk-ant-oat-OAUTH-CANARY-12345";
const APIKEY_CANARY: &str = "sk-ant-api-KEY-CANARY-67890";

fn assert_no_canary(calls: &[Call]) {
    for call in calls {
        for (name, value) in &call.headers {
            assert!(
                !value.contains("CANARY"),
                "credential leaked to the gateway: header {name} on {}",
                call.path
            );
        }
        let body = String::from_utf8_lossy(&call.body);
        assert!(
            !body.contains("CANARY"),
            "credential leaked into a gateway body on {}",
            call.path
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn credentials_never_reach_the_gateway_and_the_optimized_body_reaches_anthropic() {
    let anth_rec = Recorded::new();
    let gw_rec = Recorded::new();
    let anthropic = mock_anthropic(anth_rec.clone()).await;
    let gateway = mock_gateway(gw_rec.clone(), GatewayKind::Optimizing).await;
    let relay = start(gateway, anthropic).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{}/v1/messages", relay.addr))
        .header("x-api-key", APIKEY_CANARY)
        .header("authorization", format!("Bearer {OAUTH_CANARY}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "interleaved-thinking-2025-05-14")
        .json(&original_body())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let _ = res.bytes().await.unwrap();

    // hard isolation: the gateway saw the lwllm key and NOTHING else
    let gw_calls = gw_rec.snapshot();
    assert!(!gw_calls.is_empty());
    assert_no_canary(&gw_calls);
    let optimize = &gw_calls[0];
    assert_eq!(optimize.path, "/anthropic/v1/optimize");
    assert!(optimize
        .headers
        .iter()
        .any(|(k, v)| k == "x-leeway-api-key" && v == "lwllm_relay_test_key"));
    assert!(!optimize
        .headers
        .iter()
        .any(|(k, _)| k == "authorization" || k == "x-api-key" || k.starts_with("anthropic-")));
    let envelope: serde_json::Value = serde_json::from_slice(&optimize.body).unwrap();
    assert_eq!(
        envelope.pointer("/meta/mode").and_then(|v| v.as_str()),
        Some("safe")
    );
    assert_eq!(
        envelope.pointer("/meta/sessionId").and_then(|v| v.as_str()),
        Some("relay-it-session")
    );
    assert_eq!(
        envelope.pointer("/meta/deviceId").and_then(|v| v.as_str()),
        Some("dev-relay-it")
    );
    assert_eq!(
        envelope
            .pointer("/request/messages/0/content")
            .and_then(|v| v.as_str()),
        Some("the original prompt")
    );

    // anthropic got the held credential + the OPTIMIZED body
    let messages_call = anth_rec
        .snapshot()
        .into_iter()
        .find(|c| c.path == "/v1/messages")
        .expect("anthropic /v1/messages was called");
    assert!(messages_call
        .headers
        .iter()
        .any(|(k, v)| k == "x-api-key" && v == APIKEY_CANARY));
    assert!(messages_call
        .headers
        .iter()
        .any(|(k, v)| k == "authorization" && v.contains(OAUTH_CANARY)));
    assert!(messages_call
        .headers
        .iter()
        .any(|(k, v)| k == "anthropic-beta" && v.contains("interleaved")));
    let sent: serde_json::Value = serde_json::from_slice(&messages_call.body).unwrap();
    assert_eq!(
        sent.pointer("/messages/0/content").and_then(|v| v.as_str()),
        Some("OPTIMIZED")
    );
    assert_eq!(
        sent.pointer("/metadata/user_id").and_then(|v| v.as_str()),
        Some("u1")
    );

    // actuals + locally-counted baseline land on the gateway, async
    wait_for("the actuals report", || gw_rec.count() >= 2).await;
    let gw_calls = gw_rec.snapshot();
    assert_no_canary(&gw_calls);
    let actuals = gw_calls
        .iter()
        .find(|c| c.path == "/anthropic/v1/optimize/req_TEST1/actuals")
        .expect("actuals reported");
    let payload: serde_json::Value = serde_json::from_slice(&actuals.body).unwrap();
    assert_eq!(
        payload
            .pointer("/usage/input_tokens")
            .and_then(|v| v.as_u64()),
        Some(120)
    );
    assert_eq!(
        payload
            .pointer("/usage/output_tokens")
            .and_then(|v| v.as_u64()),
        Some(40)
    );
    assert_eq!(
        payload.pointer("/baselineTokens").and_then(|v| v.as_u64()),
        Some(1234)
    );
    assert_eq!(
        payload.pointer("/baselineSource").and_then(|v| v.as_str()),
        Some("provider_count")
    );
    assert_eq!(
        payload.pointer("/providerStatus").and_then(|v| v.as_u64()),
        Some(200)
    );

    // the baseline was counted on the ORIGINAL body, with the held credential
    let count_call = anth_rec
        .snapshot()
        .into_iter()
        .find(|c| c.path == "/v1/messages/count_tokens")
        .expect("local count_tokens ran");
    assert!(count_call
        .headers
        .iter()
        .any(|(k, v)| k == "x-api-key" && v == APIKEY_CANARY));
    let count_body: serde_json::Value = serde_json::from_slice(&count_call.body).unwrap();
    assert_eq!(
        count_body
            .pointer("/messages/0/content")
            .and_then(|v| v.as_str()),
        Some("the original prompt")
    );
    assert!(
        count_body.get("max_tokens").is_none(),
        "generation params are stripped from the count body"
    );

    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_bytes_reach_the_client_byte_identical() {
    let anth_rec = Recorded::new();
    let gw_rec = Recorded::new();
    let anthropic = mock_anthropic(anth_rec.clone()).await;
    let gateway = mock_gateway(gw_rec.clone(), GatewayKind::Optimizing).await;
    let relay = start(gateway, anthropic).await;

    let mut body = original_body();
    body["stream"] = serde_json::json!(true);
    let res = reqwest::Client::new()
        .post(format!("http://{}/v1/messages", relay.addr))
        .header("x-api-key", APIKEY_CANARY)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(res
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("event-stream"));
    let bytes = res.bytes().await.unwrap();
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        SSE_FIXTURE,
        "SSE must pass through untouched"
    );

    // usage parsed from the tee'd branch
    wait_for("the SSE actuals report", || {
        gw_rec
            .snapshot()
            .iter()
            .any(|c| c.path.ends_with("/actuals"))
    })
    .await;
    let actuals = gw_rec
        .snapshot()
        .into_iter()
        .find(|c| c.path.ends_with("/actuals"))
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&actuals.body).unwrap();
    assert_eq!(
        payload
            .pointer("/usage/input_tokens")
            .and_then(|v| v.as_u64()),
        Some(100)
    );
    assert_eq!(
        payload
            .pointer("/usage/output_tokens")
            .and_then(|v| v.as_u64()),
        Some(55)
    );
    assert_eq!(
        payload
            .pointer("/usage/cache_read_input_tokens")
            .and_then(|v| v.as_u64()),
        Some(25)
    );
    assert_eq!(
        payload
            .pointer("/usage/cache_creation_input_tokens")
            .and_then(|v| v.as_u64()),
        Some(10)
    );

    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gateway_down_fails_open_with_the_original_body() {
    let anth_rec = Recorded::new();
    let anthropic = mock_anthropic(anth_rec.clone()).await;
    // a port nothing listens on — connection refused, fast
    let relay = start("http://127.0.0.1:1".to_string(), anthropic).await;

    let res = reqwest::Client::new()
        .post(format!("http://{}/v1/messages", relay.addr))
        .header("x-api-key", APIKEY_CANARY)
        .json(&original_body())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "the session must never block on Leeway");
    let call = anth_rec
        .snapshot()
        .into_iter()
        .find(|c| c.path == "/v1/messages")
        .unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&call.body).unwrap();
    assert_eq!(
        sent.pointer("/messages/0/content").and_then(|v| v.as_str()),
        Some("the original prompt"),
        "fail-open must forward the ORIGINAL body"
    );
    assert_eq!(
        relay
            .stats
            .fail_open
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_402_switches_to_pure_passthrough_after_one_gateway_call() {
    let anth_rec = Recorded::new();
    let gw_rec = Recorded::new();
    let anthropic = mock_anthropic(anth_rec.clone()).await;
    let gateway = mock_gateway(gw_rec.clone(), GatewayKind::PlanRequired).await;
    let relay = start(gateway, anthropic).await;

    let client = reqwest::Client::new();
    for _ in 0..2 {
        let res = client
            .post(format!("http://{}/v1/messages", relay.addr))
            .header("x-api-key", APIKEY_CANARY)
            .json(&original_body())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }
    assert_eq!(
        gw_rec.count(),
        1,
        "after the 402 the gateway is not contacted again"
    );
    assert_eq!(
        anth_rec
            .snapshot()
            .iter()
            .filter(|c| c.path == "/v1/messages")
            .count(),
        2
    );
    assert!(relay
        .stats
        .plan_blocked
        .load(std::sync::atomic::Ordering::Relaxed));
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn count_tokens_and_unknown_paths_bypass_the_gateway_entirely() {
    let anth_rec = Recorded::new();
    let gw_rec = Recorded::new();
    let anthropic = mock_anthropic(anth_rec.clone()).await;
    let gateway = mock_gateway(gw_rec.clone(), GatewayKind::Optimizing).await;
    let relay = start(gateway, anthropic).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{}/v1/messages/count_tokens", relay.addr))
        .header("x-api-key", APIKEY_CANARY)
        .json(&serde_json::json!({"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"count"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.json::<serde_json::Value>().await.unwrap()["input_tokens"],
        1234
    );

    let res2 = client
        .get(format!("http://{}/v1/models?limit=3", relay.addr))
        .header("x-api-key", APIKEY_CANARY)
        .send()
        .await
        .unwrap();
    assert_eq!(res2.status(), 200);

    assert_eq!(
        gw_rec.count(),
        0,
        "the gateway is never involved in passthrough paths"
    );
    let count_call = anth_rec
        .snapshot()
        .into_iter()
        .find(|c| c.path == "/v1/messages/count_tokens")
        .unwrap();
    assert!(count_call
        .headers
        .iter()
        .any(|(k, v)| k == "x-api-key" && v == APIKEY_CANARY));
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn eight_concurrent_requests_all_stream_correctly() {
    let anth_rec = Recorded::new();
    let gw_rec = Recorded::new();
    let anthropic = mock_anthropic(anth_rec.clone()).await;
    let gateway = mock_gateway(gw_rec.clone(), GatewayKind::Optimizing).await;
    let relay = start(gateway, anthropic).await;
    let addr = relay.addr;

    let client = reqwest::Client::new();
    let mut tasks = Vec::new();
    for i in 0..8 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            let mut body = original_body();
            body["stream"] = serde_json::json!(true);
            body["messages"][0]["content"] = serde_json::json!(format!("request {i}"));
            let res = client
                .post(format!("http://{addr}/v1/messages"))
                .header("x-api-key", APIKEY_CANARY)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200);
            let bytes = res.bytes().await.unwrap();
            assert_eq!(std::str::from_utf8(&bytes).unwrap(), SSE_FIXTURE);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(
        anth_rec
            .snapshot()
            .iter()
            .filter(|c| c.path == "/v1/messages")
            .count(),
        8
    );
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_stops_accepting_connections() {
    let anth_rec = Recorded::new();
    let gw_rec = Recorded::new();
    let anthropic = mock_anthropic(anth_rec).await;
    let gateway = mock_gateway(gw_rec, GatewayKind::Optimizing).await;
    let relay = start(gateway, anthropic).await;
    let addr = relay.addr;
    relay.shutdown().await;
    let err = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .timeout(Duration::from_secs(1))
        .json(&original_body())
        .send()
        .await;
    assert!(err.is_err(), "the relay must stop with the child");
}
