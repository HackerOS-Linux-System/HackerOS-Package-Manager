use anyhow::Result;
use colored::Colorize;
use std::fs;
use std::process::Command;
use crate::utils::{download_file, compare_versions};

const VERSION_URL: &str = "https://raw.githubusercontent.com/HackerOS-Linux-System/Hacker-Package-Manager/main/version.hacker";
const LOCAL_VERSION_FILE: &str = "/usr/lib/HackerOS/hpm/version.json";
const RELEASES_BASE: &str = "https://github.com/HackerOS-Linux-System/Hacker-Package-Manager/releases/download/v";

pub fn upgrade() -> Result<()> {
    let lock = crate::utils::acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| crate::utils::release_lock());

    let tmp_version = "/tmp/hpm-version.hacker";
    download_file(VERSION_URL, tmp_version)?;
    let remote_raw = fs::read_to_string(tmp_version)?;
    let remote_version = remote_raw.trim().to_string();

    let local_version = if fs::metadata(LOCAL_VERSION_FILE).is_ok() {
        let data = fs::read_to_string(LOCAL_VERSION_FILE)?;
        let v: serde_json::Value = serde_json::from_str(&data)?;
        v["version"].as_str().unwrap_or("0.0").to_string()
    } else {
        "0.0".to_string()
    };

    if compare_versions(&remote_version, &local_version) == std::cmp::Ordering::Greater {
        println!("{} Upgrading HPM from {} to {}...", "→".yellow(), local_version.cyan(), remote_version.cyan());

        let hpm_url = format!("{}{}/hpm", RELEASES_BASE, remote_version);
        let backend_url = format!("{}{}/backend", RELEASES_BASE, remote_version);

        download_file(&hpm_url, "/usr/bin/hpm")?;
        download_file(&backend_url, "/usr/lib/HackerOS/hpm/backend")?;

        Command::new("chmod").args(&["+x", "/usr/bin/hpm"]).status()?;
        Command::new("chmod").args(&["+x", "/usr/lib/HackerOS/hpm/backend"]).status()?;

        let new_state = serde_json::json!({ "version": remote_version });
        fs::write(LOCAL_VERSION_FILE, new_state.to_string())?;

        println!("{} Upgrade complete to version {}", "✔".green(), remote_version.green());
    } else {
        println!("{} HPM is already up to date.", "✔".green());
    }

    Ok(())
}
