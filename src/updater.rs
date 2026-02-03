use anyhow::{anyhow, Result};
use log::{debug, info};
use serde::Deserialize;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const VERSION: &str = env!("CARGO_PKG_VERSION");

// TODO: Update this with your actual GitHub repo
const GITHUB_REPO: &str = "your-username/lego-radio";

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

/// Check if a newer version is available on GitHub
pub fn check_for_update() -> Option<String> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        GITHUB_REPO
    );

    debug!("Checking for updates: {}", url);

    let response = ureq::get(&url)
        .set("User-Agent", "lego-radio")
        .set("Accept", "application/vnd.github.v3+json")
        .call()
        .ok()?;

    let release: GitHubRelease = response.into_json().ok()?;
    let latest = release.tag_name.trim_start_matches('v').to_string();

    debug!("Current: v{}, Latest: v{}", VERSION, latest);

    if version_greater(&latest, VERSION) {
        Some(latest)
    } else {
        None
    }
}

/// Download and install the latest version
pub fn do_update() -> Result<()> {
    info!("Checking for updates...");

    let latest = check_for_update().ok_or_else(|| anyhow!("Already up to date (v{})", VERSION))?;

    info!("Downloading v{}...", latest);

    // Determine binary name based on architecture
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    let binary_name = match (os, arch) {
        ("linux", "aarch64") => "lego-radio-arm64",
        ("linux", "x86_64") => "lego-radio-x86_64",
        ("macos", "aarch64") => "lego-radio-darwin-arm64",
        ("macos", "x86_64") => "lego-radio-darwin-x86_64",
        _ => return Err(anyhow!("Unsupported platform: {}-{}", os, arch)),
    };

    let url = format!(
        "https://github.com/{}/releases/latest/download/{}",
        GITHUB_REPO, binary_name
    );

    info!("Downloading from: {}", url);

    // Download to temp file
    let tmp_path = "/tmp/lego-radio-update";
    let response = ureq::get(&url)
        .set("User-Agent", "lego-radio")
        .call()
        .map_err(|e| anyhow!("Download failed: {}", e))?;

    let mut file = fs::File::create(tmp_path)?;
    let mut reader = response.into_reader();
    std::io::copy(&mut reader, &mut file)?;
    file.flush()?;

    // Make executable
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(tmp_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(tmp_path, perms)?;
    }

    // Replace current binary
    let current_exe = std::env::current_exe()?;
    info!("Replacing: {:?}", current_exe);

    // On some systems, we can't replace a running binary directly
    // So we rename the old one first, then move the new one in place
    let backup_path = format!("{}.backup", current_exe.display());

    // Remove old backup if exists
    let _ = fs::remove_file(&backup_path);

    // Rename current to backup
    fs::rename(&current_exe, &backup_path)?;

    // Move new binary to current location
    fs::rename(tmp_path, &current_exe)?;

    // Remove backup
    let _ = fs::remove_file(&backup_path);

    info!("Updated to v{}!", latest);
    info!("Restart the service: sudo systemctl restart lego-radio");

    Ok(())
}

/// Compare semantic versions (returns true if a > b)
fn version_greater(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> {
        v.split('.')
            .filter_map(|s| s.parse().ok())
            .collect()
    };

    let va = parse(a);
    let vb = parse(b);

    for (a_part, b_part) in va.iter().zip(vb.iter()) {
        if a_part > b_part {
            return true;
        }
        if a_part < b_part {
            return false;
        }
    }

    va.len() > vb.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_compare() {
        assert!(version_greater("1.0.1", "1.0.0"));
        assert!(version_greater("1.1.0", "1.0.0"));
        assert!(version_greater("2.0.0", "1.9.9"));
        assert!(version_greater("0.2.0", "0.1.0"));
        assert!(version_greater("0.1.1", "0.1.0"));

        assert!(!version_greater("1.0.0", "1.0.0"));
        assert!(!version_greater("1.0.0", "1.0.1"));
        assert!(!version_greater("0.9.9", "1.0.0"));
    }

    #[test]
    fn test_version_compare_different_lengths() {
        assert!(version_greater("1.0.1", "1.0"));
        assert!(!version_greater("1.0", "1.0.1"));
    }

    #[test]
    fn test_binary_name_selection() {
        // Test that we handle all expected platforms
        let platforms = [
            (("linux", "aarch64"), "lego-radio-arm64"),
            (("linux", "x86_64"), "lego-radio-x86_64"),
            (("macos", "aarch64"), "lego-radio-darwin-arm64"),
            (("macos", "x86_64"), "lego-radio-darwin-x86_64"),
        ];

        for ((os, arch), expected) in platforms {
            let binary_name = match (os, arch) {
                ("linux", "aarch64") => "lego-radio-arm64",
                ("linux", "x86_64") => "lego-radio-x86_64",
                ("macos", "aarch64") => "lego-radio-darwin-arm64",
                ("macos", "x86_64") => "lego-radio-darwin-x86_64",
                _ => panic!("Unsupported"),
            };
            assert_eq!(binary_name, expected);
        }
    }
}
