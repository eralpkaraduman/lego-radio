use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const GITHUB_REPO: &str = "eralpkaraduman/lego-radio";

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
/// If version is None, checks for update first. If Some, uses provided version.
pub fn do_update_to(version: Option<&str>) -> Result<()> {
    let latest = match version {
        Some(v) => v.to_string(),
        None => {
            info!("Checking for updates...");
            check_for_update().ok_or_else(|| anyhow!("Already up to date (v{})", VERSION))?
        }
    };

    info!("Downloading v{}...", latest);

    // Determine binary name based on architecture
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    let binary_name = get_binary_name(os, arch)
        .ok_or_else(|| anyhow!("Unsupported platform: {}-{}", os, arch))?;

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

    // Verify SHA256 checksum (if available)
    let checksum_url = format!("{}.sha256", url);
    match ureq::get(&checksum_url)
        .set("User-Agent", "lego-radio")
        .call()
    {
        Ok(resp) => {
            let mut checksum_body = String::new();
            resp.into_reader()
                .read_to_string(&mut checksum_body)
                .map_err(|e| anyhow!("Failed to read checksum: {}", e))?;
            let expected = checksum_body.split_whitespace().next().unwrap_or("");

            let binary_data = fs::read(tmp_path)?;
            let actual = format!("{:x}", Sha256::digest(&binary_data));

            if actual != expected {
                fs::remove_file(tmp_path)?;
                return Err(anyhow!(
                    "Checksum mismatch: expected {} got {}",
                    expected,
                    actual
                ));
            }
            info!("Checksum verified: {}", actual);
        }
        Err(_) => {
            warn!("No checksum file available, skipping verification");
        }
    }

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
    // So we rename the old one first, then copy the new one in place
    // (copy instead of rename to handle cross-filesystem updates)
    let backup_path = format!("{}.backup", current_exe.display());

    // Remove old backup if exists
    let _ = fs::remove_file(&backup_path);

    // Rename current to backup (same filesystem, should work)
    fs::rename(&current_exe, &backup_path)?;

    // Copy new binary to current location (handles cross-filesystem)
    fs::copy(tmp_path, &current_exe)?;

    // Set executable permissions
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&current_exe)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&current_exe, perms)?;
    }

    // Clean up temp and backup files
    let _ = fs::remove_file(tmp_path);
    let _ = fs::remove_file(&backup_path);

    info!("Updated to v{}!", latest);
    info!("Restart the service: sudo systemctl restart lego-radio");

    Ok(())
}

/// Download and install the latest version (checks for update first)
pub fn do_update() -> Result<()> {
    do_update_to(None)
}

/// Get binary name for a given OS/architecture combination
fn get_binary_name(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "aarch64") => Some("lego-radio-arm64"),
        _ => None,
    }
}

/// Compare semantic versions (returns true if a > b)
fn version_greater(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> { v.split('.').filter_map(|s| s.parse().ok()).collect() };

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
    use std::io::Read;

    // =============================================================================
    // Version Comparison Tests
    // =============================================================================

    #[test]
    fn test_version_compare_patch() {
        assert!(version_greater("1.0.1", "1.0.0"));
        assert!(version_greater("0.1.1", "0.1.0"));
        assert!(version_greater("0.0.2", "0.0.1"));
    }

    #[test]
    fn test_version_compare_minor() {
        assert!(version_greater("1.1.0", "1.0.0"));
        assert!(version_greater("1.2.0", "1.1.9"));
        assert!(version_greater("0.2.0", "0.1.0"));
    }

    #[test]
    fn test_version_compare_major() {
        assert!(version_greater("2.0.0", "1.9.9"));
        assert!(version_greater("2.0.0", "1.0.0"));
        assert!(version_greater("10.0.0", "9.9.9"));
    }

    #[test]
    fn test_version_compare_equal() {
        assert!(!version_greater("1.0.0", "1.0.0"));
        assert!(!version_greater("0.0.1", "0.0.1"));
        assert!(!version_greater("99.99.99", "99.99.99"));
    }

    #[test]
    fn test_version_compare_older() {
        assert!(!version_greater("1.0.0", "1.0.1"));
        assert!(!version_greater("0.9.9", "1.0.0"));
        assert!(!version_greater("1.0.0", "2.0.0"));
    }

    #[test]
    fn test_version_compare_different_lengths() {
        assert!(version_greater("1.0.1", "1.0"));
        assert!(!version_greater("1.0", "1.0.1"));
        assert!(version_greater("1.0.0.1", "1.0.0"));
    }

    #[test]
    fn test_version_compare_with_leading_zeros() {
        // Parser handles leading zeros gracefully
        assert!(!version_greater("1.0.0", "1.00.00"));
    }

    // =============================================================================
    // Binary Name Selection Tests
    // =============================================================================

    #[test]
    fn test_binary_name_selection() {
        // Test all supported platforms
        assert_eq!(
            get_binary_name("linux", "aarch64"),
            Some("lego-radio-arm64")
        );

        // Only arm64 Linux is supported
        assert_eq!(get_binary_name("linux", "x86_64"), None);
        assert_eq!(get_binary_name("macos", "aarch64"), None);
        assert_eq!(get_binary_name("windows", "x86_64"), None);
    }

    // =============================================================================
    // File Operation Tests (using temp directories)
    // =============================================================================

    #[test]
    fn test_cross_filesystem_copy() {
        // Simulate the update flow with temp files
        let temp_dir = std::env::temp_dir();
        let source = temp_dir.join("test_source_binary");
        let dest = temp_dir.join("test_dest_binary");

        // Create source file with some content
        fs::write(&source, b"test binary content").unwrap();

        // Copy (not rename) to handle cross-filesystem
        fs::copy(&source, &dest).unwrap();

        // Verify content matches
        let mut source_content = Vec::new();
        let mut dest_content = Vec::new();
        fs::File::open(&source)
            .unwrap()
            .read_to_end(&mut source_content)
            .unwrap();
        fs::File::open(&dest)
            .unwrap()
            .read_to_end(&mut dest_content)
            .unwrap();

        assert_eq!(source_content, dest_content);

        // Cleanup
        let _ = fs::remove_file(&source);
        let _ = fs::remove_file(&dest);
    }

    #[test]
    fn test_backup_and_restore_flow() {
        let temp_dir = std::env::temp_dir();
        let binary_path = temp_dir.join("test_binary");
        let backup_path = temp_dir.join("test_binary.backup");
        let new_binary = temp_dir.join("test_new_binary");

        // Create "current" binary
        fs::write(&binary_path, b"old version").unwrap();

        // Create "new" binary
        fs::write(&new_binary, b"new version").unwrap();

        // Simulate update flow
        let _ = fs::remove_file(&backup_path);
        fs::rename(&binary_path, &backup_path).unwrap();
        fs::copy(&new_binary, &binary_path).unwrap();

        // Verify new content
        let content = fs::read_to_string(&binary_path).unwrap();
        assert_eq!(content, "new version");

        // Verify backup exists
        let backup_content = fs::read_to_string(&backup_path).unwrap();
        assert_eq!(backup_content, "old version");

        // Cleanup
        let _ = fs::remove_file(&binary_path);
        let _ = fs::remove_file(&backup_path);
        let _ = fs::remove_file(&new_binary);
    }

    #[test]
    #[cfg(unix)]
    fn test_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = std::env::temp_dir();
        let binary_path = temp_dir.join("test_executable");

        // Create file
        fs::write(&binary_path, b"#!/bin/bash\necho test").unwrap();

        // Set executable permissions
        let mut perms = fs::metadata(&binary_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&binary_path, perms).unwrap();

        // Verify permissions
        let perms = fs::metadata(&binary_path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o755);

        // Cleanup
        let _ = fs::remove_file(&binary_path);
    }

    // =============================================================================
    // GitHub API Response Tests
    // =============================================================================

    #[test]
    fn test_github_release_deserialization() {
        let json = r#"{"tag_name": "v1.2.3"}"#;
        let release: GitHubRelease = serde_json::from_str(json).unwrap();
        assert_eq!(release.tag_name, "v1.2.3");
    }

    #[test]
    fn test_github_release_tag_parsing() {
        let json = r#"{"tag_name": "v1.2.3"}"#;
        let release: GitHubRelease = serde_json::from_str(json).unwrap();
        let version = release.tag_name.trim_start_matches('v');
        assert_eq!(version, "1.2.3");
    }

    #[test]
    fn test_github_release_without_v_prefix() {
        let json = r#"{"tag_name": "1.2.3"}"#;
        let release: GitHubRelease = serde_json::from_str(json).unwrap();
        let version = release.tag_name.trim_start_matches('v');
        assert_eq!(version, "1.2.3");
    }

    // =============================================================================
    // URL Construction Tests
    // =============================================================================

    #[test]
    fn test_github_api_url() {
        let url = format!(
            "https://api.github.com/repos/{}/releases/latest",
            GITHUB_REPO
        );
        assert_eq!(
            url,
            "https://api.github.com/repos/eralpkaraduman/lego-radio/releases/latest"
        );
    }

    #[test]
    fn test_download_url_construction() {
        let binary_name = "lego-radio-arm64";
        let url = format!(
            "https://github.com/{}/releases/latest/download/{}",
            GITHUB_REPO, binary_name
        );
        assert_eq!(
            url,
            "https://github.com/eralpkaraduman/lego-radio/releases/latest/download/lego-radio-arm64"
        );
    }

    // =============================================================================
    // Constants Tests
    // =============================================================================

    #[test]
    fn test_version_constant_valid() {
        // VERSION should be a valid semver
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert!(parts.len() >= 2, "Version should have at least major.minor");

        for part in parts {
            assert!(
                part.parse::<u32>().is_ok(),
                "Version part '{}' should be numeric",
                part
            );
        }
    }

    #[test]
    fn test_github_repo_format() {
        assert!(
            GITHUB_REPO.contains('/'),
            "GITHUB_REPO should be in 'owner/repo' format"
        );
        let parts: Vec<&str> = GITHUB_REPO.split('/').collect();
        assert_eq!(parts.len(), 2, "GITHUB_REPO should have exactly one '/'");
    }
}
