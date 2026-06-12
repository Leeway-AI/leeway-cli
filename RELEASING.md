# Releasing the leeway CLI

Releases are fully automated by [cargo-dist](https://opensource.axo.dev/cargo-dist/) (`dist-workspace.toml` + the generated `.github/workflows/release.yml`). Pushing a `v*` tag builds all five targets, creates the GitHub Release with archives + installers, and pushes the Homebrew formula.

## What a release produces

| Artifact | Serves |
|---|---|
| `leeway-cli-installer.sh` | `curl -fsSL https://leewayai.app/install.sh \| bash` |
| `leeway-cli-installer.ps1` | `irm https://leewayai.app/install.ps1 \| iex` |
| `leeway.rb` → pushed to `Leeway-AI/homebrew-tap` | `brew install leeway-ai/tap/leeway` |
| `leeway-cli-{x86_64,aarch64}-apple-darwin.tar.gz` | macOS archives (also consumed by `leeway update`) |
| `leeway-cli-{x86_64,aarch64}-unknown-linux-gnu.tar.gz` | Linux archives |
| `leeway-cli-x86_64-pc-windows-msvc.zip` | Windows archive |
| `sha256.sum` + per-artifact `.sha256` | checksums |

Archives are `.tar.gz` (not `.xz`) on purpose: `leeway update` (self_update + flate2) extracts them.

## Required repository secrets

| Secret | Why |
|---|---|
| `HOMEBREW_TAP_TOKEN` | a fine-grained PAT with **write access to `Leeway-AI/homebrew-tap`** (contents: read/write). Used by the `publish-homebrew-formula` job to push `Formula/leeway.rb`. |

`GITHUB_TOKEN` (automatic) covers everything else.

## First-release runbook

1. **Create the tap repo**: `Leeway-AI/homebrew-tap`, public, with an empty `Formula/` directory (a README is enough to initialize it).
2. **Create the PAT** with write access to that repo and add it to `Leeway-AI/leeway-cli` → Settings → Secrets → Actions as `HOMEBREW_TAP_TOKEN`.
3. **Point the website at the installers** (leeway-site `vercel.json` — snippet below) so the `leewayai.app/install.sh|ps1` one-liners resolve.
4. Tag and push:
   ```sh
   git tag v1.0.0
   git push --tags
   ```
5. Watch the `Release` workflow (plan → build matrix → host → publish-homebrew → announce).
6. Verify all three install commands on a clean machine/VM:
   ```sh
   curl -fsSL https://leewayai.app/install.sh | bash
   brew install leeway-ai/tap/leeway
   irm https://leewayai.app/install.ps1 | iex
   ```
   then `leeway --version` and `leeway update` (should answer "already up to date").

## leeway-site redirects (do NOT edit that repo from here — paste into its vercel.json)

```json
{
  "redirects": [
    {
      "source": "/install.sh",
      "destination": "https://github.com/Leeway-AI/leeway-cli/releases/latest/download/leeway-cli-installer.sh",
      "permanent": false
    },
    {
      "source": "/install.ps1",
      "destination": "https://github.com/Leeway-AI/leeway-cli/releases/latest/download/leeway-cli-installer.ps1",
      "permanent": false
    }
  ]
}
```

## Day-to-day releases

1. Bump `version` in `Cargo.toml`, commit.
2. `dist plan` locally for a sanity check (its output is also kept in `dist-plan-output.txt`).
3. `git tag vX.Y.Z && git push --tags`.

Changing dist config? Edit `dist-workspace.toml`, then run `dist generate` and commit the regenerated `release.yml` (the workflow checks for drift and fails the release if you forget).
