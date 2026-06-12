//! Environment construction + cross-platform spawn.
//!
//! Verified against the Claude Code docs (code.claude.com/docs/en/env-vars):
//!   - ANTHROPIC_BASE_URL  "Override the API endpoint to route requests
//!     through a proxy or gateway."
//!   - ANTHROPIC_API_KEY   "API key sent as X-Api-Key header. When set, this
//!     key is used instead of your Claude subscription even if you are logged
//!     in." → managed mode sets it to the lwllm_ key (the gateway accepts it
//!     via x-api-key); subscription mode must NOT set it.
//!   - ANTHROPIC_CUSTOM_HEADERS  "Name: Value format, newline-separated for
//!     multiple headers."

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What to inject into / strip from the child's environment, plus the
/// warnings to print (pre-existing values we override for the child only).
#[derive(Debug, Default)]
pub struct ChildEnv {
    pub set: Vec<(String, String)>,
    pub remove: Vec<String>,
    pub warnings: Vec<String>,
}

fn custom_headers(mode: &str, session: &str, version: &str, device: &str) -> String {
    // newline-separated "Name: Value" pairs, per the documented format
    let mut headers =
        format!("x-optimize-mode: {mode}\nx-session-id: {session}\nx-leeway-cli: {version}");
    if !device.is_empty() {
        // plan seat accounting: how many devices use this account at once
        headers.push_str(&format!("\nx-leeway-device: {device}"));
    }
    headers
}

fn warn_overrides(env: &mut ChildEnv, get: &dyn Fn(&str) -> Option<String>, names: &[&str]) {
    for name in names {
        if get(name).is_some() {
            env.warnings.push(format!("{name} is already set in your environment — overriding it for the child process only"));
        }
    }
}

/// managed mode: Claude Code → gateway (/anthropic) → api.anthropic.com.
pub fn managed_env(
    gateway: &str,
    api_key: &str,
    mode: &str,
    session: &str,
    version: &str,
    device: &str,
    get: &dyn Fn(&str) -> Option<String>,
) -> ChildEnv {
    let mut env = ChildEnv::default();
    warn_overrides(
        &mut env,
        get,
        &[
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
        ],
    );
    env.set.push((
        "ANTHROPIC_BASE_URL".into(),
        format!("{}/anthropic", gateway.trim_end_matches('/')),
    ));
    env.set
        .push(("ANTHROPIC_API_KEY".into(), api_key.to_string()));
    env.set.push((
        "ANTHROPIC_CUSTOM_HEADERS".into(),
        custom_headers(mode, session, version, device),
    ));
    // an auth token would shadow the injected key
    if get("ANTHROPIC_AUTH_TOKEN").is_some() {
        env.remove.push("ANTHROPIC_AUTH_TOKEN".into());
    }
    env
}

/// subscription mode: Claude Code → localhost relay. NOTHING auth-related is
/// set — the user's claude.ai OAuth login must stay in effect (a present
/// ANTHROPIC_API_KEY would override the subscription per the docs, so a
/// pre-existing one is removed for the child, with a warning).
pub fn subscription_env(port: u16, get: &dyn Fn(&str) -> Option<String>) -> ChildEnv {
    let mut env = ChildEnv::default();
    warn_overrides(&mut env, get, &["ANTHROPIC_BASE_URL"]);
    env.set.push((
        "ANTHROPIC_BASE_URL".into(),
        format!("http://127.0.0.1:{port}"),
    ));
    for name in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
        if get(name).is_some() {
            env.warnings.push(format!(
                "{name} is set in your environment — removing it for the child so your claude.ai subscription login is used"
            ));
            env.remove.push(name.into());
        }
    }
    env
}

// ---------------------------------------------------------------------------
// spawn
// ---------------------------------------------------------------------------

pub fn resolve_target(name: &str) -> Result<PathBuf> {
    which::which(name).map_err(|_| {
        if name == "claude" {
            anyhow!(
                "could not find `claude` on your PATH.\n\nInstall Claude Code first:\n  npm install -g @anthropic-ai/claude-code\n  (or: curl -fsSL https://claude.ai/install.sh | bash)\nthen run `leeway launch claude` again."
            )
        } else {
            anyhow!("could not find `{name}` on your PATH")
        }
    })
}

#[cfg(windows)]
fn build_command(path: &Path, args: &[String]) -> Command {
    use std::os::windows::process::CommandExt;
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "cmd" || ext == "bat" {
        // npm shims are batch files — they must run through cmd.exe. With /s,
        // cmd strips the outer quotes and runs the inner command line as-is;
        // each piece is quoted with standard C-runtime rules so the shim's %*
        // re-expansion reaches node intact (spaces + embedded quotes survive).
        fn quote(s: &str) -> String {
            let mut out = String::with_capacity(s.len() + 2);
            out.push('"');
            let mut backslashes = 0usize;
            for ch in s.chars() {
                match ch {
                    '\\' => {
                        backslashes += 1;
                        out.push('\\');
                    }
                    '"' => {
                        out.push_str(&"\\".repeat(backslashes + 1));
                        out.push('"');
                        backslashes = 0;
                    }
                    _ => {
                        backslashes = 0;
                        out.push(ch);
                    }
                }
            }
            out.push_str(&"\\".repeat(backslashes));
            out.push('"');
            out
        }
        let mut line = quote(&path.to_string_lossy());
        for arg in args {
            line.push(' ');
            line.push_str(&quote(arg));
        }
        let mut cmd = Command::new("cmd");
        cmd.arg("/d").arg("/s");
        cmd.raw_arg(format!("/c \"{line}\""));
        cmd
    } else {
        let mut cmd = Command::new(path);
        cmd.args(args);
        cmd
    }
}

#[cfg(not(windows))]
fn build_command(path: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new(path);
    cmd.args(args);
    cmd
}

/// Spawns the target with inherited stdio, ignores Ctrl-C in the parent while
/// the child runs (the child gets the signal), waits, and returns the child's
/// exact exit code.
pub fn spawn_and_wait(path: &Path, args: &[String], env: &ChildEnv) -> Result<i32> {
    let mut cmd = build_command(path, args);
    for name in &env.remove {
        cmd.env_remove(name);
    }
    for (name, value) in &env.set {
        cmd.env(name, value);
    }
    // interactive TUI: the child owns the terminal
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    // Ctrl-C must reach the child, not kill leeway before it can print the
    // summary — best-effort (a second registration in-process would error)
    let _ = ctrlc::set_handler(|| {});
    let status = cmd
        .status()
        .with_context(|| format!("failed to start {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Ok(128 + signal);
        }
    }
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    fn get<'a>(env: &'a ChildEnv, key: &str) -> Option<&'a str> {
        env.set
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn managed_env_injects_gateway_key_and_headers() {
        let existing = env_of(&[]);
        let env = managed_env(
            "http://localhost:8787/",
            "lwllm_abc",
            "balanced",
            "sess-1",
            "0.1.0",
            "dev-42",
            &existing,
        );
        assert_eq!(
            get(&env, "ANTHROPIC_BASE_URL"),
            Some("http://localhost:8787/anthropic")
        );
        assert_eq!(get(&env, "ANTHROPIC_API_KEY"), Some("lwllm_abc"));
        let headers = get(&env, "ANTHROPIC_CUSTOM_HEADERS").unwrap();
        assert_eq!(
            headers,
            "x-optimize-mode: balanced\nx-session-id: sess-1\nx-leeway-cli: 0.1.0\nx-leeway-device: dev-42"
        );
        assert!(env.warnings.is_empty());
    }

    #[test]
    fn managed_env_omits_the_device_header_when_unknown() {
        let existing = env_of(&[]);
        let env = managed_env("https://gw", "lwllm_x", "safe", "s", "0.1.0", "", &existing);
        let headers = get(&env, "ANTHROPIC_CUSTOM_HEADERS").unwrap();
        assert!(!headers.contains("x-leeway-device"));
    }

    #[test]
    fn managed_env_warns_and_overrides_existing_values() {
        let existing = env_of(&[
            ("ANTHROPIC_BASE_URL", "https://other"),
            ("ANTHROPIC_AUTH_TOKEN", "tok"),
        ]);
        let env = managed_env("https://gw", "lwllm_x", "safe", "s", "0.1.0", "d", &existing);
        assert_eq!(env.warnings.len(), 2);
        assert!(env.remove.contains(&"ANTHROPIC_AUTH_TOKEN".to_string()));
    }

    #[test]
    fn subscription_env_sets_localhost_and_nothing_auth_related() {
        let existing = env_of(&[]);
        let env = subscription_env(43210, &existing);
        assert_eq!(
            get(&env, "ANTHROPIC_BASE_URL"),
            Some("http://127.0.0.1:43210")
        );
        // the hard invariant: no key injection — the OAuth login must win
        assert!(get(&env, "ANTHROPIC_API_KEY").is_none());
        assert!(get(&env, "ANTHROPIC_AUTH_TOKEN").is_none());
        assert!(get(&env, "ANTHROPIC_CUSTOM_HEADERS").is_none());
        assert!(env.remove.is_empty());
    }

    #[test]
    fn subscription_env_strips_a_preexisting_api_key() {
        let existing = env_of(&[("ANTHROPIC_API_KEY", "sk-ant-x")]);
        let env = subscription_env(1234, &existing);
        assert!(env.remove.contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(env.warnings.iter().any(|w| w.contains("subscription")));
    }
}
