use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// TTS engine selection (determined once at boot)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TtsEngine {
    /// Native piper binary (Linux, or macOS with working binary)
    Piper,
    /// Docker-based piper (macOS fallback)
    #[cfg(target_os = "macos")]
    DockerPiper,
    /// macOS say command (last resort fallback)
    #[cfg(target_os = "macos")]
    MacSay,
    /// No TTS available
    None,
}

/// Piper TTS wrapper that downloads and manages the piper binary and voice model
pub struct PiperTts {
    piper_dir: PathBuf,
    piper_path: PathBuf,
    model_path: PathBuf,
    /// Which TTS engine to use (checked once at boot)
    engine: TtsEngine,
}

impl PiperTts {
    /// Create a new PiperTts instance, downloading required files if needed
    /// Tests TTS capability once at boot and remembers the result
    pub fn new() -> Result<Self> {
        // Check if piper is installed system-wide (e.g., in Docker on Pi)
        if let Some(mut tts) = Self::try_system_piper()? {
            if tts.test_piper() {
                info!("Using system-installed piper");
                tts.engine = TtsEngine::Piper;
                return Ok(tts);
            }
        }

        // Fall back to downloading piper
        let data_dir = get_data_dir()?;
        fs::create_dir_all(&data_dir)?;

        let piper_dir = data_dir.join("piper");
        let piper_path = piper_dir.join(get_piper_binary_name());
        let model_path = data_dir.join("en_US-lessac-medium.onnx");

        let mut tts = Self {
            piper_dir,
            piper_path,
            model_path,
            engine: TtsEngine::None,
        };

        // Download voice model if needed (used by all engines)
        if !tts.model_path.exists() {
            tts.download_voice()?;
        }

        // Download native piper if needed
        if !tts.piper_path.exists() {
            tts.download_piper()?;
        }

        // Test engines in order of preference
        if tts.test_piper() {
            info!("TTS engine: Piper (native)");
            tts.engine = TtsEngine::Piper;
        } else {
            #[cfg(target_os = "macos")]
            {
                if tts.test_docker_piper() {
                    info!("TTS engine: Piper (Docker)");
                    tts.engine = TtsEngine::DockerPiper;
                } else if tts.test_say() {
                    info!("TTS engine: macOS say");
                    tts.engine = TtsEngine::MacSay;
                } else {
                    warn!("No TTS engine available");
                    tts.engine = TtsEngine::None;
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                warn!("Piper not working, TTS disabled");
                tts.engine = TtsEngine::None;
            }
        }

        Ok(tts)
    }

    /// Test if native piper works
    fn test_piper(&self) -> bool {
        debug!("Testing native Piper...");
        match self.synthesize_with_piper("test") {
            Ok(samples) => {
                debug!("Native Piper test successful, got {} samples", samples.len());
                !samples.is_empty()
            }
            Err(e) => {
                debug!("Native Piper test failed: {}", e);
                false
            }
        }
    }

    /// Test if Docker piper works
    #[cfg(target_os = "macos")]
    fn test_docker_piper(&self) -> bool {
        debug!("Testing Docker Piper...");

        // Check if Docker is available
        if Command::new("docker").arg("--version").output().is_err() {
            debug!("Docker not available");
            return false;
        }

        // Check if our piper image exists
        let output = Command::new("docker")
            .args(["images", "-q", "lego-radio-piper"])
            .output();

        match output {
            Ok(o) if !o.stdout.is_empty() => {
                debug!("Docker Piper image found");
                // Test synthesis
                match self.synthesize_with_docker_piper("test") {
                    Ok(samples) => {
                        debug!("Docker Piper test successful, got {} samples", samples.len());
                        !samples.is_empty()
                    }
                    Err(e) => {
                        debug!("Docker Piper test failed: {}", e);
                        false
                    }
                }
            }
            _ => {
                info!("Docker Piper image not found. Build it with: docker build -f Dockerfile.piper -t lego-radio-piper .");
                false
            }
        }
    }

    /// Test if macOS say works
    #[cfg(target_os = "macos")]
    fn test_say(&self) -> bool {
        debug!("Testing macOS say...");
        Command::new("say").arg("test").output().is_ok()
    }

    /// Try to use system-installed piper (e.g., in Docker on Pi)
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
                    engine: TtsEngine::None,
                }));
            }
        }

        Ok(None)
    }

    /// Synthesize text to raw audio samples (16-bit PCM, 22050 Hz, mono)
    /// Uses the TTS engine determined at boot
    pub fn synthesize(&self, text: &str) -> Result<Vec<i16>> {
        match self.engine {
            TtsEngine::Piper => self.synthesize_with_piper(text),
            #[cfg(target_os = "macos")]
            TtsEngine::DockerPiper => self.synthesize_with_docker_piper(text),
            #[cfg(target_os = "macos")]
            TtsEngine::MacSay => self.synthesize_with_say(text),
            TtsEngine::None => Err(anyhow!("No TTS engine available")),
        }
    }

    /// Synthesize using native Piper binary
    fn synthesize_with_piper(&self, text: &str) -> Result<Vec<i16>> {
        debug!("Piper TTS: {}", text);

        let mut cmd = Command::new(&self.piper_path);
        cmd.current_dir(&self.piper_dir)
            .args(["--model", self.model_path.to_str().unwrap(), "--output-raw"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Set library path
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

    /// Synthesize using Docker Piper (for macOS)
    #[cfg(target_os = "macos")]
    fn synthesize_with_docker_piper(&self, text: &str) -> Result<Vec<i16>> {
        debug!("Docker Piper TTS: {}", text);

        let data_dir = get_data_dir()?;

        let mut child = Command::new("docker")
            .args([
                "run", "--rm", "-i",
                "-v", &format!("{}:/data", data_dir.display()),
                "lego-radio-piper",
                "--model", "/data/en_US-lessac-medium.onnx",
                "--output-raw",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("Failed to run docker piper: {}", e))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }

        let output = child.wait_with_output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("Docker Piper failed: {}", stderr));
        }

        // Convert bytes to i16 samples (little-endian)
        let samples: Vec<i16> = output
            .stdout
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();

        Ok(samples)
    }

    /// Speak using macOS say command (blocking, plays directly)
    #[cfg(target_os = "macos")]
    fn synthesize_with_say(&self, text: &str) -> Result<Vec<i16>> {
        debug!("macOS say: {}", text);
        let status = Command::new("say")
            .arg(text)
            .status()
            .map_err(|e| anyhow!("Failed to run say: {}", e))?;

        if !status.success() {
            return Err(anyhow!("say command failed"));
        }

        // Return empty samples - audio was already played by say
        Ok(vec![])
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
        ("macos", "x86_64") => "piper_macos_x64.tar.gz",
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
