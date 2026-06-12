//! clap surface. Hard rule: the CLI NEVER parses, validates or reorders the
//! target's arguments — everything after the target name is forwarded
//! verbatim. Leeway's own flags are only valid BETWEEN `launch` and the
//! target (clap's trailing_var_arg + allow_hyphen_values guarantee it).

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "leeway",
    version,
    about = "Launch your coding agent through the LeewayLLM gateway — same model, less context, receipts for every request.",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in via your browser — opens the dashboard, no key copy-paste
    Auth {
        /// Gateway base URL (default: the stored one, else https://api.leewayai.app)
        #[arg(long)]
        gateway: Option<String>,
        /// How your Claude Code is billed: managed (API key / BYOK) or subscription (claude.ai login)
        #[arg(long, value_enum)]
        auth: Option<AuthChoice>,
    },
    /// Store an lwllm_ key manually (self-host / CI; `leeway auth` is easier)
    Login {
        /// LeewayLLM API key (lwllm_…). Prompted (hidden) when omitted.
        #[arg(long)]
        key: Option<String>,
        /// Gateway base URL (default: the stored one, else https://api.leewayai.app)
        #[arg(long)]
        gateway: Option<String>,
        /// How your Claude Code is billed: managed (API key / BYOK) or subscription (claude.ai login)
        #[arg(long, value_enum)]
        auth: Option<AuthChoice>,
    },
    /// Launch a target (v1: claude) with gateway optimization wired in
    Launch(LaunchArgs),
    /// Gateway health + account/plan/trial info
    Status,
    /// Read or write the config file
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Self-update from the latest GitHub release
    Update,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum AuthChoice {
    /// API-billed / BYOK: traffic goes Claude Code → gateway → Anthropic (full $ savings)
    Managed,
    /// claude.ai subscription login: localhost relay, your credential never leaves this machine
    Subscription,
}

impl AuthChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthChoice::Managed => "managed",
            AuthChoice::Subscription => "subscription",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "managed" => Some(AuthChoice::Managed),
            "subscription" => Some(AuthChoice::Subscription),
            _ => None,
        }
    }
}

#[derive(Args, Debug)]
pub struct LaunchArgs {
    /// managed | subscription (default: config; asked once on a TTY)
    #[arg(long, value_enum)]
    pub auth: Option<AuthChoice>,
    /// off | safe | balanced | aggressive (default: config, else safe)
    #[arg(long)]
    pub mode: Option<String>,
    /// Session id for grouped savings (default: a fresh UUID per launch)
    #[arg(long)]
    pub session: Option<String>,
    /// Gateway base URL override
    #[arg(long)]
    pub gateway: Option<String>,
    /// Print what would be injected/started, without spawning
    #[arg(long)]
    pub print_env: bool,
    /// Skip the end-of-session savings summary
    #[arg(long)]
    pub no_summary: bool,
    /// The target and its arguments — forwarded VERBATIM, never parsed
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
    pub target: Vec<String>,
}

impl LaunchArgs {
    /// (binary, verbatim argv for it)
    pub fn split_target(&self) -> (&str, &[String]) {
        (&self.target[0], &self.target[1..])
    }
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the whole config, or one key
    Get { key: Option<String> },
    /// Set one key (gateway_url, default_mode, default_auth, subscription_ack, auto_update)
    Set { key: String, value: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("parse")
    }

    #[test]
    fn leeway_flags_only_between_launch_and_target() {
        let cli = parse(&[
            "leeway",
            "launch",
            "--auth",
            "managed",
            "--mode",
            "off",
            "--session",
            "s1",
            "claude",
            "--resume",
            "abc",
            "-p",
            "fix the bug",
        ]);
        let Command::Launch(l) = cli.cmd else {
            panic!("expected launch")
        };
        assert_eq!(l.auth, Some(AuthChoice::Managed));
        assert_eq!(l.mode.as_deref(), Some("off"));
        let (target, args) = l.split_target();
        assert_eq!(target, "claude");
        assert_eq!(args, ["--resume", "abc", "-p", "fix the bug"]);
    }

    #[test]
    fn target_args_named_like_leeway_flags_are_forwarded_verbatim() {
        // "--mode" AFTER the target belongs to the target, not to leeway
        let cli = parse(&[
            "leeway",
            "launch",
            "claude",
            "--mode",
            "ludicrous",
            "--print-env",
            "--auth",
            "x",
        ]);
        let Command::Launch(l) = cli.cmd else {
            panic!()
        };
        assert_eq!(l.mode, None);
        assert!(!l.print_env);
        let (target, args) = l.split_target();
        assert_eq!(target, "claude");
        assert_eq!(args, ["--mode", "ludicrous", "--print-env", "--auth", "x"]);
    }

    #[test]
    fn target_arg_literally_named_dash_dash_mode_first() {
        let cli = parse(&["leeway", "launch", "claude", "--mode"]);
        let Command::Launch(l) = cli.cmd else {
            panic!()
        };
        let (target, args) = l.split_target();
        assert_eq!(target, "claude");
        assert_eq!(args, ["--mode"]);
        assert_eq!(l.mode, None);
    }

    #[test]
    fn launch_requires_a_target() {
        assert!(Cli::try_parse_from(["leeway", "launch", "--mode", "safe"]).is_err());
    }
}
