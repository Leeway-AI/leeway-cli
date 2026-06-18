//! Config file: `~/.config/leeway/config.toml` (unix, perms 600) or
//! `%APPDATA%\leeway\config.toml` (windows). `LEEWAY_CONFIG_DIR` overrides the
//! directory (used by tests and unusual setups).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_KEYS: &[&str] = &[
    "gateway_url",
    "api_key",
    "default_mode",
    "default_auth",
    "subscription_ack",
    "auto_update",
];
/// The hosted gateway. Self-hosters: `leeway config set gateway_url http://localhost:8787`.
pub const DEFAULT_GATEWAY: &str = "https://api.leewayai.app";

/// Dashboard origin for human-facing links: the hosted gateway pairs with the
/// leewayai.app dashboard; self-hosted gateways serve an embedded one at /app.
pub fn dashboard_url(gateway: &str) -> String {
    let g = gateway.trim_end_matches('/');
    if g == DEFAULT_GATEWAY {
        "https://leewayai.app".to_string()
    } else {
        g.to_string()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub gateway_url: Option<String>,
    pub api_key: Option<String>,
    /// off | safe | balanced | aggressive
    pub default_mode: Option<String>,
    /// managed | subscription
    pub default_auth: Option<String>,
    /// one-time acknowledgement of the subscription-mode notice
    #[serde(default)]
    pub subscription_ack: bool,
    /// self-update from GitHub releases automatically when the gateway
    /// advertises a newer version (default: notify only)
    #[serde(default)]
    pub auto_update: bool,
}

impl Config {
    pub fn gateway(&self) -> String {
        self.gateway_url
            .clone()
            .unwrap_or_else(|| DEFAULT_GATEWAY.to_string())
    }
    pub fn mode(&self) -> String {
        // balanced = the cache-safe sweet spot for coding agents: it actually
        // removes tokens from fresh tool output (trim / strip-base64 /
        // compress-dom) while the content-pure stages keep the agent's prompt
        // cache stable. aggressive is deliberately NOT the default — its
        // prune-history / diff-tool-results shift the cached prefix every turn
        // (the gateway also guards against this when cache_control is present).
        self.default_mode
            .clone()
            .unwrap_or_else(|| "balanced".to_string())
    }
}

pub fn is_valid_mode(mode: &str) -> bool {
    matches!(mode, "off" | "safe" | "balanced" | "aggressive")
}

pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("LEEWAY_CONFIG_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    #[cfg(windows)]
    {
        let base = std::env::var("APPDATA").context("APPDATA is not set")?;
        Ok(PathBuf::from(base).join("leeway"))
    }
    #[cfg(not(windows))]
    {
        let base = match std::env::var("XDG_CONFIG_HOME") {
            Ok(x) if !x.is_empty() => PathBuf::from(x),
            _ => {
                let home = std::env::var("HOME").context("HOME is not set")?;
                PathBuf::from(home).join(".config")
            }
        };
        Ok(base.join("leeway"))
    }
}

/// Stable anonymous device id, generated once and persisted next to the
/// config. Lets the gateway count SIMULTANEOUS devices on one account (plan
/// seats: Pro 1, Max 5). Pure random token — no hardware or personal data.
pub fn device_id() -> Result<String> {
    device_id_from(&config_dir()?)
}

pub fn device_id_from(dir: &Path) -> Result<String> {
    let path = dir.join("device_id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    let id = format!("dev-{}", uuid::Uuid::new_v4().simple());
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&path, &id).with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

/// True at most once per `interval_secs` — the version check never spams the
/// gateway. The timestamp lives next to the config; recording happens even
/// when the check later fails (no retry storms).
pub fn update_check_due(dir: &Path, interval_secs: u64) -> bool {
    let path = dir.join("update_check");
    let last: u64 = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    now_secs().saturating_sub(last) >= interval_secs
}

pub fn record_update_check(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join("update_check"), now_secs().to_string());
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "0.2.0" vs "0.1.9" — true when `candidate` is strictly newer. Tolerates a
/// leading "v"; anything unparsable compares as 0 (never a false positive
/// from garbage).
pub fn is_newer_version(candidate: &str, current: &str) -> bool {
    let parse = |s: &str| -> [u64; 3] {
        let mut parts = [0u64; 3];
        for (i, p) in s
            .trim()
            .trim_start_matches('v')
            .split('.')
            .take(3)
            .enumerate()
        {
            parts[i] = p.parse().unwrap_or(0);
        }
        parts
    };
    parse(candidate) > parse(current)
}

pub fn load() -> Result<Config> {
    load_from(&config_dir()?)
}

pub fn load_from(dir: &Path) -> Result<Config> {
    let path = dir.join("config.toml");
    if !path.exists() {
        return Ok(Config::default());
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

pub fn save(cfg: &Config) -> Result<()> {
    save_to(&config_dir()?, cfg)
}

pub fn save_to(dir: &Path, cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("config.toml");
    let body = toml::to_string_pretty(cfg).context("serializing config")?;
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    // the file holds the lwllm_ key — owner-only on unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let empty = load_from(dir.path()).unwrap();
        assert!(empty.api_key.is_none());
        assert_eq!(empty.gateway(), DEFAULT_GATEWAY);
        assert_eq!(empty.mode(), "balanced");
        assert!(!empty.subscription_ack);

        let cfg = Config {
            gateway_url: Some("https://gw.example.com".into()),
            api_key: Some("lwllm_test123".into()),
            default_mode: Some("safe".into()),
            default_auth: Some("subscription".into()),
            subscription_ack: true,
            auto_update: false,
        };
        save_to(dir.path(), &cfg).unwrap();
        let back = load_from(dir.path()).unwrap();
        assert_eq!(back.gateway_url.as_deref(), Some("https://gw.example.com"));
        assert_eq!(back.api_key.as_deref(), Some("lwllm_test123"));
        assert_eq!(back.default_auth.as_deref(), Some("subscription"));
        assert!(back.subscription_ack);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("config.toml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn version_compare_and_check_throttle() {
        assert!(is_newer_version("0.2.0", "0.1.0"));
        assert!(is_newer_version("v1.0.0", "0.9.9"));
        assert!(!is_newer_version("0.1.0", "0.1.0"));
        assert!(!is_newer_version("0.1.0", "0.2.0"));
        assert!(!is_newer_version("garbage", "0.1.0")); // unparsable never nags

        let dir = tempfile::tempdir().unwrap();
        assert!(update_check_due(dir.path(), 86_400)); // never checked
        record_update_check(dir.path());
        assert!(!update_check_due(dir.path(), 86_400)); // throttled
        assert!(update_check_due(dir.path(), 0)); // zero interval = always due
    }

    #[test]
    fn device_id_is_generated_once_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let a = device_id_from(dir.path()).unwrap();
        let b = device_id_from(dir.path()).unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with("dev-"));
        assert!(a.len() > 10);
        // a different config dir = a different device
        let other = tempfile::tempdir().unwrap();
        assert_ne!(a, device_id_from(other.path()).unwrap());
    }

    #[test]
    fn validates_modes() {
        for m in ["off", "safe", "balanced", "aggressive"] {
            assert!(is_valid_mode(m));
        }
        assert!(!is_valid_mode("turbo"));
    }
}
