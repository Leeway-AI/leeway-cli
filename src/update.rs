//! `leeway update` — replace the running binary from the latest GitHub
//! release. The ONLY network call here is to GitHub releases, and only on
//! explicit request (or auto_update = true).
//!
//! cargo-dist archives are .tar.gz with the binary inside a top-level
//! directory named after the artifact (verified against a real release):
//!   leeway-cli-x86_64-pc-windows-msvc/leeway.exe
//! self_update must be pointed at that inner path explicitly.

use anyhow::{Context, Result};

pub const GITHUB_OWNER: &str = "Leeway-AI";
pub const GITHUB_REPO: &str = "leeway-cli";

/// Path of the binary INSIDE a cargo-dist archive for the given target.
pub fn archive_bin_path(target: &str) -> String {
    let bin = if target.contains("windows") {
        "leeway.exe"
    } else {
        "leeway"
    };
    format!("leeway-cli-{target}/{bin}")
}

pub fn run() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let target = self_update::get_target();
    let status = self_update::backends::github::Update::configure()
        .repo_owner(GITHUB_OWNER)
        .repo_name(GITHUB_REPO)
        .bin_name("leeway")
        .bin_path_in_archive(&archive_bin_path(target))
        .target(target)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_path_matches_the_real_archive_layout() {
        // verified by listing leeway-cli-x86_64-pc-windows-msvc.tar.gz (v1.0.2)
        assert_eq!(
            archive_bin_path("x86_64-pc-windows-msvc"),
            "leeway-cli-x86_64-pc-windows-msvc/leeway.exe"
        );
        assert_eq!(
            archive_bin_path("aarch64-apple-darwin"),
            "leeway-cli-aarch64-apple-darwin/leeway"
        );
    }
}
