# LeewayLLM CLI

Launch your coding agent through the [LeewayLLM](https://leewayai.app) gateway: **same model, same provider — minus the context your agent should never have sent.** Every request gets a receipt; your dashboard shows exactly what was measured.

## Install

```sh
curl -fsSL https://leewayai.app/install.sh | bash        # macOS / Linux
brew install leeway-ai/tap/leeway                        # Homebrew
irm https://leewayai.app/install.ps1 | iex               # Windows (PowerShell)
```

## Quickstart

```sh
leeway auth                  # sign in via your browser — no key copy-paste
leeway launch claude         # that's it — flags after `claude` go to claude untouched
leeway launch claude --resume abc -p "fix the bug"
```

`leeway auth` opens your dashboard, you press **Approve this device**, done.
If you belong to an organization, the approval page lets you pick which account
the device uses (the org — billed to its shared plan — or your personal one);
it defaults to the org. The key is named after this device, so re-authorizing
the same machine replaces its key instead of piling up.
(Manual alternative: `leeway login --key lwllm_…` — handy for CI and self-host.)

Leeway's own flags go **between** `launch` and the target; everything after the target is forwarded verbatim:

```sh
leeway launch --mode aggressive --session my-task claude --resume abc
```

Example session summary:

```
$ leeway launch claude
…
Session: 142 requests · direct est $3.81 → leeway $0.97 · saved $2.84 (−74%)
details: https://leewayai.app/app/usage
```

## The two modes

| | **managed** (recommended) | **subscription** |
|---|---|---|
| Your Claude Code billing | API key / BYOK (Anthropic Console key stored in Leeway) | Your claude.ai Pro/Max/Team/Enterprise login |
| Traffic | Claude Code → Leeway gateway → Anthropic (server-side) | Claude Code → **localhost relay** → Anthropic, from your machine |
| Your Anthropic credential | The gateway uses the BYOK key you stored | **Never leaves your machine** |
| Savings | Real $ savings, receipts, full audit | Notional only (you pay a flat Leeway plan; $0 billed savings) |
| Stretch your claude.ai plan caps | n/a | Yes — fewer tokens per request |

Pick once at `leeway auth` (or `leeway config set default_auth managed|subscription`); override per launch with `--auth`.

## Subscription mode — how it works

- **Your OAuth credential and every call to Anthropic stay on your machine.** Leeway's gateway only receives request content for optimization and never your token — the relay hard-strips `Authorization`, `x-api-key` and all `anthropic-*` headers before anything touches the gateway (and the gateway rejects any provider credential with a 400).
- A paid Leeway plan (or the developer trial) is required for the gateway's optimization endpoints. If the gateway is unreachable or says no, the relay **fails open**: your session continues directly against Anthropic, unoptimized, never blocked.

Trade-off in one line: *managed = real savings & receipts, server-side; subscription = privacy of your credential & plan-cap stretching, savings are notional.*

## Commands

| Command | What it does |
|---|---|
| `leeway auth [--gateway URL] [--auth managed\|subscription]` | Browser sign-in: approve the device in your dashboard, the key arrives by itself |
| `leeway login [--key lwllm_…] [--gateway URL] [--auth managed\|subscription]` | Manual key entry (CI / self-host) |
| `leeway launch <target> [args…]` | Spawn the target wired to the gateway (v1 target: `claude`) |
| `leeway status` | Gateway health, plan, trial, BYOK providers, month figures |
| `leeway config get [key]` / `leeway config set <key> <value>` | `gateway_url`, `default_mode`, `default_auth`, `subscription_ack`, `auto_update` |
| `leeway update` | Self-update from the latest GitHub release (`auto_update = true` does it for you) |

### `leeway launch` flags (between `launch` and the target)

- `--auth managed|subscription` — billing mode for this launch
- `--mode off|safe|balanced|aggressive` — optimization mode (default: config, else `safe`)
- `--session <id>` — group requests under one session (default: a fresh UUID per launch)
- `--gateway <url>` — gateway override
- `--print-env` — show what would be injected/started, spawn nothing
- `--no-summary` — skip the end-of-session savings line

## Config

`~/.config/leeway/config.toml` (unix, mode 600) or `%APPDATA%\leeway\config.toml`:

```toml
gateway_url = "https://api.leewayai.app"
api_key = "lwllm_…"
default_mode = "safe"
default_auth = "managed"
subscription_ack = false
auto_update = false   # true = self-update automatically when your gateway advertises a new release
```

Env knobs: `LEEWAY_CONFIG_DIR` (config location), `LEEWAY_OPTIMIZE_TIMEOUT_MS` (relay → gateway budget, default 10000), `LEEWAY_ANTHROPIC_URL` (relay upstream override, tests only).

A `device_id` file lives next to the config: a random anonymous token generated
once (no hardware or personal data). It lets the gateway count how many devices
use one account **at the same time** — plans include simultaneous-device seats
(Pro 1, Max 5 — extra seats purchasable; a seat frees after ~30 min idle). Over the limit, requests get
a clear 429 in managed mode; in subscription mode sessions simply continue
unoptimized until a seat frees.

No telemetry. The CLI's only network calls: your configured gateway, `api.anthropic.com` (subscription relay), and GitHub releases for updates. Version discovery asks **your gateway** (never a third party) at most once a day and just prints a nudge; GitHub is contacted only when you run `leeway update` — or automatically if you opted into `auto_update = true`.

## Troubleshooting

- **Windows SmartScreen** flags the unsigned binary on first run → "More info" → "Run anyway" (or install via the PowerShell one-liner, which unblocks the file).
- **`could not find claude on your PATH`** → install Claude Code first: `npm install -g @anthropic-ai/claude-code`.
- **Corporate proxies**: the relay binds 127.0.0.1 only and respects `HTTPS_PROXY` for its outbound calls; if your proxy intercepts TLS to `api.anthropic.com`, trust its CA system-wide.
- **"gateway unreachable"** in subscription mode is not fatal — sessions fail open straight to Anthropic (the summary shows how many requests skipped optimization). In managed mode the gateway *is* the path, so check `leeway status`.
- **One-time prompt in Claude Code (managed mode)**: with `ANTHROPIC_API_KEY` set, interactive Claude Code asks once whether to use the key instead of your subscription — answer yes; that key is your `lwllm_` gateway key (Claude Code's masked display shows a generic `sk-ant-…` prefix — check the key's tail).
