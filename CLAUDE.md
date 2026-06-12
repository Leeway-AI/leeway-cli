# leeway CLI (Rust)

Single binary `leeway`: launches coding agents (v1 target: `claude`) through
the LeewayLLM gateway. Two auth modes — **managed** (env injection, traffic
goes through the gateway server-side) and **subscription** (localhost relay;
the user's claude.ai OAuth credential and the Anthropic call never leave the
machine). The backend lives in the sibling repo
`C:\Users\ayoub\Documents\Leeway\LeewayLLM`.

## Commands

```
cargo test                                # unit + integration (relay mocks, fake-claude spawns)
cargo clippy --all-targets -- -D warnings # must stay clean
cargo fmt --check                         # must stay clean
cargo run -- launch --print-env claude    # inspect the injection plan
```

Integration tests compile `tests/helpers/fake_claude.rs` with plain rustc at
test time — no extra [[bin]] ships in releases.

## Hard invariants — never violate, never "improve away"

1. **Never parse, validate or reorder the target's arguments.** Everything
   after the target name is forwarded verbatim (clap `trailing_var_arg` +
   `allow_hyphen_values`; tested with a target arg literally named `--mode`).
   Leeway flags are only valid BETWEEN `launch` and the target.
2. **Subscription credentials never reach the gateway.** The relay strips and
   holds `Authorization`, `x-api-key` and every `anthropic-*` header; only the
   `x-leeway-api-key` (lwllm) travels to `/anthropic/v1/optimize`. The relay
   integration tests hard-assert no credential canary appears in any gateway
   request — keep that assertion alive.
3. **Fail-open is mandatory in subscription mode.** Gateway timeout/5xx/429 →
   the ORIGINAL body goes straight to Anthropic; a 402 prints the upgrade hint
   once and switches the session to pure passthrough. The user's session is
   never blocked by Leeway availability.
4. **Streams are never buffered toward the client** — tee only for usage
   extraction (capped side buffer).
5. **In subscription mode, NOTHING auth-related is set on the child.** A
   pre-existing `ANTHROPIC_API_KEY` is removed for the child (it would
   override the claude.ai login per the Claude Code docs).
6. **No telemetry.** The only network calls: the configured gateway,
   api.anthropic.com (subscription relay), GitHub releases (explicit
   `leeway update`).
7. The one-time subscription ToS notice (`subscription_ack`) stays — it is the
   user's informed consent for relay mode.

## Releases

See `RELEASING.md` (cargo-dist; tag `v*`; installers + Homebrew tap
`leeway-ai/homebrew-tap`, formula `leeway`; secret `HOMEBREW_TAP_TOKEN`).
After editing `dist-workspace.toml`, run `dist generate` and commit the
regenerated `.github/workflows/release.yml`.

## Surface-sync rule (mirror of the backend's)

This CLI consumes the gateway surface: `/v1/key-info`,
`/v1/sessions/:id/summary`, `/anthropic/v1/optimize` (+ `/:auditId/actuals`),
the managed-mode ingress headers (`x-optimize-mode`, `x-session-id`,
`x-leeway-cli`), and `x-leeway-trial-remaining`. When the backend changes any
of those, this repo MUST change in the same change set: `src/api.rs` (shapes),
`src/relay.rs` (optimize/actuals contract), `src/launch.rs` (injection),
README, tests. A backend surface change without a matching CLI update is an
incomplete task — and vice versa: never change a consumed shape here without
the backend actually serving it.
