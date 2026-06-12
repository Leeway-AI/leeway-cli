//! `leeway update` — replace the running binary from the latest GitHub
//! release (cargo-dist artifacts: leeway-<target>.tar.gz / .zip). The ONLY
//! network call here is to GitHub releases, and only on explicit request.

use anyhow::{Context, Result};

pub const GITHUB_OWNER: &str = "Leeway-AI";
pub const GITHUB_REPO: &str = "leeway-cli";

pub fn run() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let status = self_update::backends::github::Update::configure()
        .repo_owner(GITHUB_OWNER)
        .repo_name(GITHUB_REPO)
        .bin_name("leeway")
        .target(self_update::get_target())
        .current_version(current)
        .show_download_progress(true)
        .no_confirm(true)
        .build()
        .context("configuring the updater")?
        .update()
        .context("update failed — you can always reinstall from https://github.com/Leeway-AI/leeway-cli/releases")?;
    match status {
        self_update::Status::UpToDate(v) => println!("leeway is already up to date (v{v})"),
        self_update::Status::Updated(v) => println!("updated leeway: v{current} → v{v}"),
    }
    Ok(())
}
