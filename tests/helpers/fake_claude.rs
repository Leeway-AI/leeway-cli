// std-only fake target, compiled by the integration tests with plain rustc.
// Writes its argv + the auth-relevant env to FAKE_CLAUDE_OUT (tab-separated)
// and exits with FAKE_CLAUDE_EXIT (default 0).
use std::io::Write;

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', "\\t")
}

fn main() {
    let out = std::env::var("FAKE_CLAUDE_OUT").expect("FAKE_CLAUDE_OUT must be set");
    let mut f = std::fs::File::create(out).expect("create FAKE_CLAUDE_OUT");
    for arg in std::env::args().skip(1) {
        writeln!(f, "ARG\t{}", esc(&arg)).unwrap();
    }
    for key in [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_CUSTOM_HEADERS",
    ] {
        match std::env::var(key) {
            Ok(v) => writeln!(f, "ENV\t{key}\t{}", esc(&v)).unwrap(),
            Err(_) => writeln!(f, "UNSET\t{key}").unwrap(),
        }
    }
    let code: i32 = std::env::var("FAKE_CLAUDE_EXIT").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    std::process::exit(code);
}
