use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use std::io::{IsTerminal, Write};
use std::time::Duration;

use leeway_cli::cli::{AuthChoice, Cli, Command, ConfigAction, LaunchArgs};
use leeway_cli::{api, config, launch, relay, update, CLI_VERSION};

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("leeway: {err:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Command::Auth { gateway, auth } => cmd_auth(gateway, auth).map(|()| 0),
        Command::Login { key, gateway, auth } => cmd_login(key, gateway, auth).map(|()| 0),
        Command::Launch(args) => cmd_launch(args),
        Command::Status => cmd_status().map(|()| 0),
        Command::Config { action } => cmd_config(action).map(|()| 0),
        Command::Update => update::run().map(|()| 0),
    }
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

/// Browser sign-in: start the flow, open the dashboard approval page, poll
/// until approved, then run the normal post-login steps. No key copy-paste.
fn cmd_auth(gateway: Option<String>, auth: Option<AuthChoice>) -> Result<()> {
    let cfg = config::load()?;
    let gateway = gateway.unwrap_or_else(|| cfg.gateway());

    let flow = api::cli_auth_start(&gateway, Duration::from_secs(10))?;
    println!("Confirm this code in your browser:  {}", flow.code);
    println!("{}", flow.verify_url);
    if open_browser(&flow.verify_url).is_err() {
        println!("(could not open a browser — paste the link above into one)");
    }
    print!("waiting for approval");
    std::io::stdout().flush().ok();

    let deadline = std::time::Instant::now() + Duration::from_secs(flow.expires_in_sec.max(60));
    let key = loop {
        if std::time::Instant::now() >= deadline {
            println!();
            bail!("sign-in timed out — run `leeway auth` again");
        }
        std::thread::sleep(Duration::from_secs(flow.interval_sec.max(1)));
        print!(".");
        std::io::stdout().flush().ok();
        match api::cli_auth_poll(&gateway, &flow.code, &flow.secret, Duration::from_secs(10))? {
            api::CliAuthPoll::Pending => continue,
            api::CliAuthPoll::Approved { api_key } => break api_key,
        }
    };
    println!();
    finish_login(cfg, gateway, key, auth)
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
    }
}

fn cmd_login(key: Option<String>, gateway: Option<String>, auth: Option<AuthChoice>) -> Result<()> {
    let cfg = config::load()?;
    let gateway = gateway.unwrap_or_else(|| cfg.gateway());

    let key = match key {
        Some(k) => k,
        None => rpassword::prompt_password("LeewayLLM API key (lwllm_…): ")
            .context("reading the key")?,
    };
    let key = key.trim().to_string();
    if !key.starts_with("lwllm_") {
        bail!("that does not look like a LeewayLLM key (expected the lwllm_ prefix)");
    }
    finish_login(cfg, gateway, key, auth)
}

/// Shared tail of `leeway auth` and `leeway login`: store the key FIRST, then
/// verify against the gateway best-effort. By the time we get here the key
/// already exists on the account — a flaky verification call (gateway
/// redeploying, transient network) must never strand it.
fn finish_login(
    mut cfg: config::Config,
    gateway: String,
    key: String,
    auth: Option<AuthChoice>,
) -> Result<()> {
    cfg.api_key = Some(key.clone());
    cfg.gateway_url = Some(gateway.clone());
    let auth = match auth {
        Some(a) => a,
        None => ask_auth_choice()?,
    };
    cfg.default_auth = Some(auth.as_str().to_string());
    config::save(&cfg)?;
    println!(
        "✓ saved {}",
        config::config_dir()?.join("config.toml").display()
    );

    let dashboard = config::dashboard_url(&gateway);
    match api::key_info(&gateway, &key, Duration::from_secs(10)) {
        Ok(info) => {
            println!(
                "✓ signed in — plan: {} · key: {}",
                info.plan, info.masked_key
            );
            if info.plan == "developer" {
                match &info.trial {
                    Some(t) if t.active => println!(
                        "  developer trial: {} requests / {} days left — upgrade for full access: {dashboard}/app/billing",
                        t.remaining_requests, t.days_left
                    ),
                    _ => println!(
                        "  the developer plan has no gateway access left — upgrade at {dashboard}/app/billing"
                    ),
                }
            }
            if auth == AuthChoice::Managed && !info.byok_providers.iter().any(|p| p == "anthropic")
            {
                println!(
                    "⚠ no Anthropic key is stored on your Leeway account. If this gateway is hosted,\n  requests will 401 — add one in Settings → Provider keys (BYOK): {dashboard}/app/settings"
                );
            }
        }
        Err(err) => {
            println!(
                "⚠ key saved, but the gateway could not confirm it right now ({err})\n  run `leeway status` in a minute to verify."
            );
        }
    }
    Ok(())
}

fn ask_auth_choice() -> Result<AuthChoice> {
    if !std::io::stdin().is_terminal() {
        bail!("no default_auth configured and no TTY to ask — run `leeway config set default_auth managed|subscription`");
    }
    println!("\nHow is your Claude Code billed?");
    println!("  (1) Claude subscription — you log into Claude Code with your claude.ai account");
    println!("  (2) API key / BYOK — an Anthropic Console key, or a key stored in Leeway");
    loop {
        print!("choice [1/2]: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        match line.trim() {
            "1" => return Ok(AuthChoice::Subscription),
            "2" => return Ok(AuthChoice::Managed),
            _ => println!("please answer 1 or 2"),
        }
    }
}

// ---------------------------------------------------------------------------
// launch
// ---------------------------------------------------------------------------

const SUBSCRIPTION_NOTICE: &str = "\
Subscription mode notice
------------------------
- Your claude.ai OAuth credential and every call to Anthropic stay ON THIS
  MACHINE. Leeway's gateway only receives request content for optimization —
  never your token.
";

/// Once-a-day version check against the USER'S OWN GATEWAY (never a third
/// party) — fail-silent, 2s budget, never blocks or delays the session
/// beyond that. auto_update=true additionally runs the GitHub self-update.
fn maybe_check_update(cfg: &config::Config, gateway: &str) {
    let Ok(dir) = config::config_dir() else {
        return;
    };
    if !config::update_check_due(&dir, 86_400) {
        return;
    }
    config::record_update_check(&dir); // even on failure — no retry storms
    let Ok(Some(latest)) = api::cli_latest(gateway, Duration::from_secs(2)) else {
        return;
    };
    if !config::is_newer_version(&latest, CLI_VERSION) {
        return;
    }
    if cfg.auto_update {
        eprintln!("[leeway] v{latest} is available — auto-updating (auto_update = true)…");
        if let Err(err) = update::run() {
            eprintln!("[leeway] auto-update failed ({err:#}) — continuing with v{CLI_VERSION}");
        }
    } else {
        eprintln!(
            "[leeway] ↑ v{latest} is available (you have v{CLI_VERSION}) — run `leeway update`, or `leeway config set auto_update true`"
        );
    }
}

fn cmd_launch(args: LaunchArgs) -> Result<i32> {
    let mut cfg = config::load()?;
    let gateway = args.gateway.clone().unwrap_or_else(|| cfg.gateway());
    maybe_check_update(&cfg, &gateway);
    let mode = args.mode.clone().unwrap_or_else(|| cfg.mode());
    if !config::is_valid_mode(&mode) {
        bail!("invalid --mode \"{mode}\" — expected off | safe | balanced | aggressive");
    }
    let session = args
        .session
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // plan seat accounting (Pro 1, Max 10 simultaneous devices) — fail-open:
    // an unwritable config dir just means the device stays unidentified
    let device = config::device_id().unwrap_or_default();

    let auth = match args
        .auth
        .or_else(|| cfg.default_auth.as_deref().and_then(AuthChoice::parse))
    {
        Some(a) => a,
        None => {
            if std::io::stdin().is_terminal() {
                let choice = ask_auth_choice()?;
                cfg.default_auth = Some(choice.as_str().to_string());
                config::save(&cfg)?;
                choice
            } else {
                bail!("no auth mode configured and no TTY to ask — run `leeway config set default_auth managed|subscription` (managed = API key / BYOK, subscription = claude.ai login)");
            }
        }
    };

    let api_key = cfg
        .api_key
        .clone()
        .ok_or_else(|| anyhow!("no API key stored — run `leeway login` first"))?;

    let (target_name, target_args) = args.split_target();
    let real_env = |name: &str| std::env::var(name).ok();

    match auth {
        AuthChoice::Managed => {
            let env = launch::managed_env(
                &gateway,
                &api_key,
                &mode,
                &session,
                CLI_VERSION,
                &device,
                &real_env,
            );
            if args.print_env {
                print_env_plan("managed", &env, target_name, target_args, None);
                return Ok(0);
            }
            for w in &env.warnings {
                eprintln!("⚠ {w}");
            }
            let path = launch::resolve_target(target_name)?;
            let code = launch::spawn_and_wait(&path, target_args, &env)?;
            if !args.no_summary {
                print_summary(&gateway, &api_key, &session, false, 0);
            }
            Ok(code)
        }
        AuthChoice::Subscription => {
            ack_subscription(&mut cfg, args.print_env)?;
            if args.print_env {
                let env = launch::subscription_env(0, &real_env);
                print_env_plan(
                    "subscription",
                    &env,
                    target_name,
                    target_args,
                    Some("a localhost relay on a dynamic 127.0.0.1 port (started at launch)"),
                );
                return Ok(0);
            }
            let path = launch::resolve_target(target_name)?;
            let rcfg = relay::RelayConfig::new(
                gateway.clone(),
                api_key.clone(),
                mode.clone(),
                session.clone(),
                device.clone(),
            );
            let rt = tokio::runtime::Runtime::new().context("starting the relay runtime")?;
            let handle = rt.block_on(relay::start_relay(rcfg))?;
            let stats = handle.stats.clone();
            let env = launch::subscription_env(handle.addr.port(), &real_env);
            for w in &env.warnings {
                eprintln!("⚠ {w}");
            }
            let code = launch::spawn_and_wait(&path, target_args, &env)?;
            rt.block_on(handle.shutdown());
            if !args.no_summary {
                let skipped = stats.fail_open.load(std::sync::atomic::Ordering::Relaxed);
                print_summary(&gateway, &api_key, &session, true, skipped);
            }
            drop(rt);
            Ok(code)
        }
    }
}

/// one-time ToS acknowledgement for subscription mode, persisted
fn ack_subscription(cfg: &mut config::Config, print_env_only: bool) -> Result<()> {
    if cfg.subscription_ack || print_env_only {
        return Ok(());
    }
    eprintln!("{SUBSCRIPTION_NOTICE}");
    if !std::io::stdin().is_terminal() {
        bail!("subscription mode needs a one-time acknowledgement — run interactively once, or `leeway config set subscription_ack true`");
    }
    print!("Type \"yes\" to acknowledge and continue: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if line.trim().eq_ignore_ascii_case("yes") {
        cfg.subscription_ack = true;
        config::save(cfg)?;
        Ok(())
    } else {
        bail!("not acknowledged — use managed mode instead (`leeway launch --auth managed …`)");
    }
}

fn print_env_plan(
    kind: &str,
    env: &launch::ChildEnv,
    target: &str,
    args: &[String],
    relay_note: Option<&str>,
) {
    println!("# leeway launch plan ({kind} mode) — nothing was spawned");
    if let Some(note) = relay_note {
        println!("# would start: {note}");
    }
    for (k, v) in &env.set {
        // never print the full key
        let shown = if k == "ANTHROPIC_API_KEY" {
            mask(v)
        } else {
            v.clone()
        };
        println!("{k}={}", shown.replace('\n', "\\n"));
    }
    for k in &env.remove {
        println!("# unset {k} (for the child only)");
    }
    for w in &env.warnings {
        println!("# warning: {w}");
    }
    println!("# would spawn: {target} {}", shell_join(args));
}

fn mask(key: &str) -> String {
    if key.len() <= 12 {
        return "lwllm_…".into();
    }
    format!("{}…{}", &key[..10], &key[key.len() - 4..])
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(' ') || a.contains('"') {
                format!("{a:?}")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn print_summary(gateway: &str, key: &str, session: &str, subscription: bool, skipped: u64) {
    // 2s budget, silent on failure — the summary must never hold the exit
    match api::session_summary(gateway, key, session, Duration::from_secs(2)) {
        Ok(s) => {
            let pct = if s.est_direct_usd > 0.0 {
                (s.saved_usd / s.est_direct_usd) * 100.0
            } else {
                0.0
            };
            // "−74%" = the bill shrank by 74%; tiny sessions can go the other way
            let pct_str = if pct >= 0.0 {
                format!("−{:.0}%", pct)
            } else {
                format!("+{:.0}%", pct.abs())
            };
            let saved = if s.saved_usd.abs() < 0.005 {
                0.0
            } else {
                s.saved_usd
            };
            let label = if subscription {
                ", notional — subscription billing"
            } else {
                ""
            };
            let mut line = format!(
                "Session: {} requests · direct est ${:.2} → leeway ${:.2} · saved ${:.2} ({pct_str}{label})",
                s.requests, s.est_direct_usd, s.leeway_usd, saved
            );
            if skipped > 0 {
                line.push_str(&format!(" · fail-open passthrough: {skipped}"));
            }
            println!("{line}");
            println!("details: {}/app/usage", config::dashboard_url(gateway));
        }
        Err(_) => {
            if skipped > 0 {
                println!("Session ended · fail-open passthrough: {skipped} (gateway unreachable for the summary)");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// status / config
// ---------------------------------------------------------------------------

fn cmd_status() -> Result<()> {
    let cfg = config::load()?;
    let gateway = cfg.gateway();
    maybe_check_update(&cfg, &gateway);
    match api::health(&gateway, Duration::from_secs(5)) {
        Ok(h) => println!(
            "gateway   {gateway} — ok (version {})",
            h.get("version").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        Err(err) => println!("gateway   {gateway} — UNREACHABLE ({err})"),
    }
    match (
        &cfg.api_key,
        cfg.api_key
            .as_ref()
            .map(|k| api::key_info(&gateway, k, Duration::from_secs(5))),
    ) {
        (Some(_), Some(Ok(info))) => {
            println!("account   plan {} · key {}", info.plan, info.masked_key);
            match info.trial {
                Some(t) => println!(
                    "trial     active={} remaining={} days_left={}",
                    t.active, t.remaining_requests, t.days_left
                ),
                None => println!("trial     n/a (paid plan)"),
            }
            println!(
                "byok      {}",
                if info.byok_providers.is_empty() {
                    "none".to_string()
                } else {
                    info.byok_providers.join(", ")
                }
            );
            println!(
                "month     {} requests · est saved ${:.2} · cost through leeway ${:.2}",
                info.month.requests, info.month.est_saved_usd, info.month.actual_cost_usd
            );
        }
        (Some(_), Some(Err(err))) => println!("account   key check failed: {err}"),
        _ => println!("account   not signed in — run `leeway login`"),
    }
    println!(
        "auth      {}",
        cfg.default_auth
            .as_deref()
            .unwrap_or("(unset — asked on first launch)")
    );
    println!("mode      {}", cfg.mode());
    Ok(())
}

fn cmd_config(action: ConfigAction) -> Result<()> {
    let mut cfg = config::load()?;
    match action {
        ConfigAction::Get { key } => {
            let print_one = |k: &str, cfg: &config::Config| -> String {
                match k {
                    "gateway_url" => cfg.gateway_url.clone().unwrap_or_default(),
                    "api_key" => cfg.api_key.as_deref().map(mask).unwrap_or_default(),
                    "default_mode" => cfg.default_mode.clone().unwrap_or_default(),
                    "default_auth" => cfg.default_auth.clone().unwrap_or_default(),
                    "subscription_ack" => cfg.subscription_ack.to_string(),
                    "auto_update" => cfg.auto_update.to_string(),
                    _ => String::new(),
                }
            };
            match key {
                Some(k) => {
                    if !config::CONFIG_KEYS.contains(&k.as_str()) {
                        bail!(
                            "unknown key \"{k}\" — known keys: {}",
                            config::CONFIG_KEYS.join(", ")
                        );
                    }
                    println!("{}", print_one(&k, &cfg));
                }
                None => {
                    for k in config::CONFIG_KEYS {
                        println!("{k} = {}", print_one(k, &cfg));
                    }
                }
            }
        }
        ConfigAction::Set { key, value } => {
            match key.as_str() {
                "gateway_url" => cfg.gateway_url = Some(value),
                "default_mode" => {
                    if !config::is_valid_mode(&value) {
                        bail!("default_mode must be off | safe | balanced | aggressive");
                    }
                    cfg.default_mode = Some(value);
                }
                "default_auth" => {
                    if AuthChoice::parse(&value).is_none() {
                        bail!("default_auth must be managed | subscription");
                    }
                    cfg.default_auth = Some(value);
                }
                "subscription_ack" => {
                    cfg.subscription_ack = value == "true" || value == "1";
                }
                "auto_update" => {
                    cfg.auto_update = value == "true" || value == "1";
                }
                "api_key" => bail!("set the key with `leeway login --key lwllm_…` (it validates against the gateway)"),
                other => bail!("unknown key \"{other}\" — known keys: {}", config::CONFIG_KEYS.join(", ")),
            }
            config::save(&cfg)?;
            println!("✓ saved");
        }
    }
    Ok(())
}
