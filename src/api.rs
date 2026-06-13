//! Blocking client for the gateway's key-authed account endpoints
//! (/v1/key-info, /v1/sessions/:id/summary, /healthz). The relay has its own
//! async client in relay.rs.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize)]
pub struct TrialInfo {
    pub active: bool,
    pub remaining_requests: i64,
    pub days_left: i64,
}

#[derive(Debug, Deserialize)]
pub struct MonthInfo {
    pub requests: i64,
    #[serde(rename = "estSavedUsd")]
    pub est_saved_usd: f64,
    #[serde(rename = "actualCostUsd")]
    pub actual_cost_usd: f64,
}

/// Present only when the key is scoped to an organization (the request bills
/// the org's shared plan). Absent = a personal key.
#[derive(Debug, Deserialize)]
pub struct KeyOrg {
    pub name: String,
    pub role: String,
}

#[derive(Debug, Deserialize)]
pub struct KeyInfo {
    pub plan: String,
    pub trial: Option<TrialInfo>,
    pub byok_providers: Vec<String>,
    pub masked_key: String,
    pub month: MonthInfo,
    #[serde(default)]
    pub org: Option<KeyOrg>,
}

#[derive(Debug, Deserialize)]
pub struct TopModel {
    pub model: String,
    pub requests: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub requests: i64,
    pub est_direct_usd: f64,
    pub leeway_usd: f64,
    pub saved_usd: f64,
    pub saved_pct: f64,
    pub top_models: Vec<TopModel>,
    pub unreported_count: i64,
}

fn client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .user_agent(format!("leeway-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building http client")
}

fn gateway_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    #[derive(Deserialize)]
    struct Err1 {
        error: ErrBody,
    }
    #[derive(Deserialize)]
    struct ErrBody {
        message: String,
    }
    let message = serde_json::from_str::<Err1>(body)
        .map(|e| e.error.message)
        .unwrap_or_else(|_| body.chars().take(200).collect());
    anyhow!("gateway returned HTTP {status}: {message}")
}

pub fn key_info(gateway: &str, key: &str, timeout: Duration) -> Result<KeyInfo> {
    let url = format!("{}/v1/key-info", gateway.trim_end_matches('/'));
    let res = client(timeout)?
        .get(url)
        .header("x-leeway-api-key", key)
        .send()
        .context("cannot reach the gateway — is it running?")?;
    let status = res.status();
    let body = res.text().unwrap_or_default();
    if !status.is_success() {
        return Err(gateway_error(status, &body));
    }
    serde_json::from_str(&body).context("unexpected /v1/key-info response shape")
}

pub fn session_summary(
    gateway: &str,
    key: &str,
    session_id: &str,
    timeout: Duration,
) -> Result<SessionSummary> {
    let url = format!(
        "{}/v1/sessions/{}/summary",
        gateway.trim_end_matches('/'),
        session_id
    );
    let res = client(timeout)?
        .get(url)
        .header("x-leeway-api-key", key)
        .send()?;
    let status = res.status();
    let body = res.text().unwrap_or_default();
    if !status.is_success() {
        return Err(gateway_error(status, &body));
    }
    serde_json::from_str(&body).context("unexpected session summary shape")
}

// ---- browser sign-in (`leeway auth`) -----------------------------------------

#[derive(Debug, Deserialize)]
pub struct CliAuthStart {
    pub code: String,
    pub secret: String,
    #[serde(rename = "verifyUrl")]
    pub verify_url: String,
    #[serde(rename = "expiresInSec")]
    pub expires_in_sec: u64,
    #[serde(rename = "intervalSec")]
    pub interval_sec: u64,
}

#[derive(Debug)]
pub enum CliAuthPoll {
    Pending,
    Approved { api_key: String },
}

pub fn cli_auth_start(gateway: &str, timeout: Duration) -> Result<CliAuthStart> {
    let url = format!("{}/app/api/cli/auth/start", gateway.trim_end_matches('/'));
    let res = client(timeout)?
        .post(url)
        .json(&serde_json::json!({ "cliVersion": env!("CARGO_PKG_VERSION") }))
        .send()
        .context("cannot reach the gateway — is it running?")?;
    let status = res.status();
    let body = res.text().unwrap_or_default();
    if !status.is_success() {
        return Err(gateway_error(status, &body));
    }
    serde_json::from_str(&body).context("unexpected sign-in start response shape")
}

pub fn cli_auth_poll(
    gateway: &str,
    code: &str,
    secret: &str,
    device: &str,
    timeout: Duration,
) -> Result<CliAuthPoll> {
    let url = format!("{}/app/api/cli/auth/poll", gateway.trim_end_matches('/'));
    // `device` lets the gateway name the key leeway-cli-<device> so re-authing
    // this machine replaces its key while other machines keep theirs.
    let res = client(timeout)?
        .post(url)
        .json(&serde_json::json!({ "code": code, "secret": secret, "device": device }))
        .send()
        .context("cannot reach the gateway — is it running?")?;
    let status = res.status();
    let body = res.text().unwrap_or_default();
    if status == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("the sign-in code expired or was already used — run `leeway auth` again");
    }
    if !status.is_success() {
        return Err(gateway_error(status, &body));
    }
    #[derive(Deserialize)]
    struct Poll {
        status: String,
        #[serde(rename = "apiKey")]
        api_key: Option<String>,
    }
    let poll: Poll =
        serde_json::from_str(&body).context("unexpected sign-in poll response shape")?;
    match (poll.status.as_str(), poll.api_key) {
        ("approved", Some(api_key)) => Ok(CliAuthPoll::Approved { api_key }),
        _ => Ok(CliAuthPoll::Pending),
    }
}

/// Latest CLI release advertised by the gateway (None = nothing advertised).
/// The ONLY destination is the user's own gateway — never a third party.
pub fn cli_latest(gateway: &str, timeout: Duration) -> Result<Option<String>> {
    let url = format!("{}/v1/cli/version", gateway.trim_end_matches('/'));
    let res = client(timeout)?.get(url).send()?;
    if !res.status().is_success() {
        return Ok(None); // older gateways don't have the endpoint — fine
    }
    #[derive(Deserialize)]
    struct V {
        latest: Option<String>,
    }
    Ok(res.json::<V>().map(|v| v.latest).unwrap_or(None))
}

pub fn health(gateway: &str, timeout: Duration) -> Result<serde_json::Value> {
    let url = format!("{}/healthz", gateway.trim_end_matches('/'));
    let res = client(timeout)?
        .get(url)
        .send()
        .context("cannot reach the gateway — is it running?")?;
    let status = res.status();
    if !status.is_success() {
        return Err(anyhow!("gateway /healthz returned HTTP {status}"));
    }
    res.json().context("non-JSON /healthz body")
}
