use anyhow::{anyhow, Result};
use log::{debug, info};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Piper TTS wrapper that downloads and manages the piper binary and voice model
pub struct PiperTts {
    piper_dir: PathBuf,
    piper_path: PathBuf,
    model_path: PathBuf,
}

impl PiperTts {
    /// Create a new PiperTts instance, downloading required files if needed
    pub fn new() -> Result<Self> {
        // Check if piper is installed system-wide (e.g., in Docker)
        if let Some(tts) = Self::try_system_piper()? {
            info!("Using system-installed piper");
            return Ok(tts);
        }

        // Fall back to downloading piper
        let data_dir = get_data_dir()?;
        fs::create_dir_all(&data_dir)?;

        let piper_dir = data_dir.join("piper");
        let piper_path = piper_dir.join(get_piper_binary_name());
        let model_path = data_dir.join("en_US-lessac-medium.onnx");

        let tts = Self {
            piper_dir,
            piper_path,
            model_path,
        };

        // Download piper if needed
        if !tts.piper_path.exists() {
            tts.download_piper()?;
        }

        // Download voice model if needed
        if !tts.model_path.exists() {
            tts.download_voice()?;
        }

        Ok(tts)
    }

    /// Try to use system-installed piper (e.g., in Docker)
    fn try_system_piper() -> Result<Option<Self>> {
        let system_paths = [
            PathBuf::from("/opt/piper"),
            PathBuf::from("/usr/local/piper"),
        ];

        for piper_dir in system_paths {
            let piper_path = piper_dir.join("piper");
            let model_path = piper_dir.join("voices/en_US-lessac-medium.onnx");

            if piper_path.exists() && model_path.exists() {
                return Ok(Some(Self {
                    piper_dir,
                    piper_path,
                    model_path,
                }));
            }
        }

        Ok(None)
    }

    /// Synthesize text to raw audio samples (16-bit PCM, 22050 Hz, mono)
    pub fn synthesize(&self, text: &str) -> Result<Vec<i16>> {
        debug!("Piper TTS: {}", text);

        let mut cmd = Command::new(&self.piper_path);
        cmd.current_dir(&self.piper_dir)
            .args(["--model", self.model_path.to_str().unwrap(), "--output-raw"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Set library path for espeak-ng
        #[cfg(target_os = "macos")]
        cmd.env("DYLD_LIBRARY_PATH", "/opt/homebrew/lib:/usr/local/lib");

        #[cfg(target_os = "linux")]
        cmd.env(
            "LD_LIBRARY_PATH",
            format!("{}:/usr/lib:/usr/local/lib", self.piper_dir.display()),
        );

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("Failed to run piper: {}", e))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }

        let output = child.wait_with_output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("Piper failed: {}", stderr));
        }

        // Convert bytes to i16 samples (little-endian)
        let samples: Vec<i16> = output
            .stdout
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();

        Ok(samples)
    }

    fn download_piper(&self) -> Result<()> {
        let url = get_piper_download_url()?;
        info!("Downloading piper from: {}", url);

        let response = ureq::get(&url)
            .set("User-Agent", "lego-radio")
            .call()
            .map_err(|e| anyhow!("Failed to download piper: {}", e))?;

        let mut data = Vec::new();
        response.into_reader().read_to_end(&mut data)?;

        // Extract tar.gz
        let decoder = flate2::read::GzDecoder::new(data.as_slice());
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(get_data_dir()?)?;

        // Make binaries executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["piper", "espeak-ng", "piper_phonemize"] {
                let path = self.piper_dir.join(name);
                if path.exists() {
                    let mut perms = fs::metadata(&path)?.permissions();
                    perms.set_mode(0o755);
                    fs::set_permissions(&path, perms)?;
                }
            }
        }

        info!("Piper installed to: {:?}", self.piper_path);
        Ok(())
    }

    fn download_voice(&self) -> Result<()> {
        let base_url =
            "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium";

        info!("Downloading voice model...");
        download_file(
            &format!("{}/en_US-lessac-medium.onnx", base_url),
            &self.model_path,
        )?;
        download_file(
            &format!("{}/en_US-lessac-medium.onnx.json", base_url),
            &self.model_path.with_extension("onnx.json"),
        )?;

        info!("Voice model installed");
        Ok(())
    }
}

fn get_data_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".local/share/lego-radio"))
}

fn get_piper_binary_name() -> &'static str {
    if cfg!(windows) {
        "piper.exe"
    } else {
        "piper"
    }
}

fn get_piper_download_url() -> Result<String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);

    let filename = match (os, arch) {
        ("linux", "aarch64") => "piper_linux_aarch64.tar.gz",
        ("linux", "x86_64") => "piper_linux_x86_64.tar.gz",
        ("linux", "arm") => "piper_linux_armv7l.tar.gz",
        ("macos", "aarch64") => "piper_macos_aarch64.tar.gz",
        ("macos", "x86_64") => "piper_macos_x86_64.tar.gz",
        _ => return Err(anyhow!("Unsupported platform: {}-{}", os, arch)),
    };

    Ok(format!(
        "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/{}",
        filename
    ))
}

fn download_file(url: &str, path: &PathBuf) -> Result<()> {
    debug!("Downloading {} to {:?}", url, path);

    let response = ureq::get(url)
        .set("User-Agent", "lego-radio")
        .call()
        .map_err(|e| anyhow!("Download failed: {}", e))?;

    let mut data = Vec::new();
    response.into_reader().read_to_end(&mut data)?;
    fs::write(path, data)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_data_dir() {
        let dir = get_data_dir().unwrap();
        assert!(dir.to_str().unwrap().contains("lego-radio"));
    }

    #[test]
    fn test_piper_download_url() {
        let url = get_piper_download_url().unwrap();
        assert!(url.starts_with("https://"));
        assert!(url.contains("piper"));
    }
}
