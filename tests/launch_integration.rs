//! End-to-end launch tests: the REAL `leeway` binary spawning a fake `claude`
//! (compiled at test time with plain rustc, std-only) that dumps its argv and
//! environment. Covers verbatim arg forwarding, env injection per auth mode,
//! exit-code propagation, and the Windows .cmd shim path.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

fn leeway_bin() -> &'static str {
    env!("CARGO_BIN_EXE_leeway")
}

/// compile tests/helpers/fake_claude.rs once per test process
fn fake_claude() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("leeway-fake-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/helpers/fake_claude.rs");
        let exe = dir.join(if cfg!(windows) {
            "fake-claude.exe"
        } else {
            "fake-claude"
        });
        let status = Command::new("rustc")
            .arg("--edition=2021")
            .arg("-o")
            .arg(&exe)
            .arg(&src)
            .status()
            .expect("rustc must be available to build the fake target");
        assert!(status.success(), "compiling fake_claude.rs failed");
        exe
    })
}

struct Dump {
    args: Vec<String>,
    env: Vec<(String, String)>,
    unset: Vec<String>,
}

fn unesc(s: &str) -> String {
    s.replace("\\t", "\t")
        .replace("\\n", "\n")
        .replace("\\\\", "\\")
}

fn read_dump(path: &Path) -> Dump {
    let raw = std::fs::read_to_string(path).expect("fake-claude wrote its dump");
    let mut dump = Dump {
        args: vec![],
        env: vec![],
        unset: vec![],
    };
    for line in raw.lines() {
        let mut parts = line.splitn(3, '\t');
        match parts.next() {
            Some("ARG") => dump.args.push(unesc(parts.next().unwrap_or(""))),
            Some("ENV") => dump.env.push((
                parts.next().unwrap_or("").to_string(),
                unesc(parts.next().unwrap_or("")),
            )),
            Some("UNSET") => dump.unset.push(parts.next().unwrap_or("").to_string()),
            _ => {}
        }
    }
    dump
}

fn env_value<'a>(dump: &'a Dump, key: &str) -> Option<&'a str> {
    dump.env
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// config dir with a stored key so launch doesn't need a live gateway
fn config_dir(auth: &str, ack: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "gateway_url = \"http://127.0.0.1:9\"\napi_key = \"lwllm_inttest123456\"\ndefault_mode = \"balanced\"\ndefault_auth = \"{auth}\"\nsubscription_ack = {ack}\n"
        ),
    )
    .unwrap();
    dir
}

fn run_leeway(
    cfg: &tempfile::TempDir,
    out: &Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> std::process::ExitStatus {
    let mut cmd = Command::new(leeway_bin());
    cmd.env("LEEWAY_CONFIG_DIR", cfg.path())
        .env("FAKE_CLAUDE_OUT", out)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.status().expect("leeway ran")
}

#[test]
fn managed_mode_injects_env_and_forwards_args_verbatim() {
    let cfg = config_dir("managed", false);
    let out = cfg.path().join("dump.txt");
    let fake = fake_claude().to_string_lossy().to_string();
    let status = run_leeway(
        &cfg,
        &out,
        &[],
        &[
            "launch",
            "--auth",
            "managed",
            "--mode",
            "off",
            "--session",
            "it-session",
            "--no-summary",
            &fake,
            "--resume",
            "abc",
            "-p",
            "fix the bug",
            "--mode",
        ],
    );
    assert!(status.success());
    let dump = read_dump(&out);
    // VERBATIM forwarding, including an arg literally named --mode
    assert_eq!(
        dump.args,
        ["--resume", "abc", "-p", "fix the bug", "--mode"]
    );
    assert_eq!(
        env_value(&dump, "ANTHROPIC_BASE_URL"),
        Some("http://127.0.0.1:9/anthropic")
    );
    assert_eq!(
        env_value(&dump, "ANTHROPIC_API_KEY"),
        Some("lwllm_inttest123456")
    );
    let headers = env_value(&dump, "ANTHROPIC_CUSTOM_HEADERS").expect("custom headers set");
    assert!(headers.contains("x-optimize-mode: off"));
    assert!(headers.contains("x-session-id: it-session"));
    assert!(headers.contains("x-leeway-cli: "));
}

#[test]
fn subscription_mode_sets_localhost_base_url_and_no_api_key() {
    let cfg = config_dir("subscription", true); // ack persisted → no prompt
    let out = cfg.path().join("dump.txt");
    let fake = fake_claude().to_string_lossy().to_string();
    let status = run_leeway(
        &cfg,
        &out,
        // a pre-existing key must be REMOVED for the child (it would override the subscription)
        &[("ANTHROPIC_API_KEY", "sk-ant-should-be-stripped")],
        &["launch", "--no-summary", &fake, "hello"],
    );
    assert!(status.success());
    let dump = read_dump(&out);
    assert_eq!(dump.args, ["hello"]);
    let base = env_value(&dump, "ANTHROPIC_BASE_URL").expect("base url set");
    assert!(base.starts_with("http://127.0.0.1:"), "got {base}");
    // the hard invariant: NOTHING auth-related is set
    assert!(
        dump.unset.contains(&"ANTHROPIC_API_KEY".to_string()),
        "ANTHROPIC_API_KEY must be absent, dump: {:?}",
        dump.env
    );
    assert!(dump.unset.contains(&"ANTHROPIC_AUTH_TOKEN".to_string()));
    assert!(env_value(&dump, "ANTHROPIC_CUSTOM_HEADERS").is_none());
}

#[test]
fn exit_code_propagates_exactly() {
    let cfg = config_dir("managed", false);
    let out = cfg.path().join("dump.txt");
    let fake = fake_claude().to_string_lossy().to_string();
    let status = run_leeway(
        &cfg,
        &out,
        &[("FAKE_CLAUDE_EXIT", "7")],
        &["launch", "--auth", "managed", "--no-summary", &fake, "x"],
    );
    assert_eq!(status.code(), Some(7));
}

#[test]
fn print_env_spawns_nothing() {
    let cfg = config_dir("managed", false);
    let out = cfg.path().join("dump.txt");
    let fake = fake_claude().to_string_lossy().to_string();
    let status = run_leeway(&cfg, &out, &[], &["launch", "--print-env", &fake, "arg1"]);
    assert!(status.success());
    assert!(!out.exists(), "print-env must not spawn the target");
}

#[cfg(windows)]
#[test]
fn windows_cmd_shim_preserves_spaces_and_quotes() {
    let cfg = config_dir("managed", false);
    let out = cfg.path().join("dump.txt");
    // an npm-style shim: a .cmd that forwards %* to the real exe
    let shim = cfg.path().join("fake-claude.cmd");
    std::fs::write(
        &shim,
        format!("@echo off\r\n\"{}\" %*\r\n", fake_claude().display()),
    )
    .unwrap();
    let shim_s = shim.to_string_lossy().to_string();
    let status = run_leeway(
        &cfg,
        &out,
        &[],
        &[
            "launch",
            "--auth",
            "managed",
            "--no-summary",
            &shim_s,
            "hello world",
            "say \"hi\"",
            "--mode",
        ],
    );
    assert!(status.success());
    let dump = read_dump(&out);
    assert_eq!(dump.args, ["hello world", "say \"hi\"", "--mode"]);
    assert_eq!(
        env_value(&dump, "ANTHROPIC_API_KEY"),
        Some("lwllm_inttest123456")
    );
}
