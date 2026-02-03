mod audio;
mod button;
mod channels;
mod tts;
mod updater;

use anyhow::Result;
use log::{error, info};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> Result<()> {
    // Initialize logger
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Parse CLI arguments
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("--version") | Some("-v") => {
            println!("lego-radio v{}", VERSION);
            return Ok(());
        }
        Some("--help") | Some("-h") => {
            print_help();
            return Ok(());
        }
        Some("--install") => {
            return install_service();
        }
        Some("--uninstall") => {
            return uninstall_service();
        }
        Some("--update") => {
            return updater::do_update();
        }
        Some("--check") => {
            return check_update();
        }
        Some("--test-tts") => {
            return test_tts();
        }
        Some("--test-stream") => {
            let url = args.get(2).map(|s| s.as_str()).unwrap_or(channels::CHANNELS[0].url);
            return test_stream(url);
        }
        _ => {}
    }

    // Run the radio
    run_radio()
}

fn print_help() {
    println!(
        r#"lego-radio v{} - LEGO Internet Radio

USAGE:
    lego-radio [OPTION]

OPTIONS:
    --version, -v     Print version
    --help, -h        Print this help
    --install         Install as systemd service (Linux)
    --uninstall       Remove systemd service (Linux)
    --update          Download and install latest version
    --check           Check for updates
    --test-tts        Test text-to-speech
    --test-stream     Test audio streaming [URL]

CONTROLS:
    On Raspberry Pi: Press the GPIO button to cycle channels
    On Mac/Desktop:  Press Enter to cycle channels

Channels cycle: 1 → 2 → 3 → ... → OFF → 1 → ...
"#,
        VERSION
    );
}

fn run_radio() -> Result<()> {
    info!("lego-radio v{} starting", VERSION);

    // Check for updates on startup
    if let Some(latest) = updater::check_for_update() {
        info!("Update available: v{} (run --update to install)", latest);
    }

    // Initialize TTS (downloads piper and voice model if needed)
    info!("Initializing TTS...");
    let tts = tts::PiperTts::new()?;

    // Current channel index (-1 = off)
    let channel_idx = Arc::new(AtomicI32::new(-1));

    // Audio player
    let mut player = audio::Player::new()?;

    // Button input (GPIO on Pi, keyboard elsewhere)
    let button = button::create_button();

    info!("Ready. Press button to start...");
    if !button.is_gpio() {
        info!("(Press Enter to simulate button press)");
    }

    loop {
        // Wait for button press
        button.wait_for_press();

        // Increment channel
        let idx = channel_idx.fetch_add(1, Ordering::SeqCst) + 1;
        let num_channels = channels::CHANNELS.len() as i32;

        if idx >= num_channels {
            // Wrap around to OFF state
            channel_idx.store(-1, Ordering::SeqCst);
            info!("Radio OFF");

            player.stop();
            player.speak("Radio off", &tts);
        } else {
            let channel = &channels::CHANNELS[idx as usize];
            info!("Channel {}: {}", idx + 1, channel.name);

            player.stop();
            player.speak(channel.tts_name, &tts);

            if let Err(e) = player.play_stream(channel.url) {
                error!("Failed to play stream: {}", e);
                player.speak("Stream error", &tts);
            }
        }
    }
}

fn check_update() -> Result<()> {
    match updater::check_for_update() {
        Some(v) => {
            println!("Update available: v{} -> v{}", VERSION, v);
            println!("Run: lego-radio --update");
        }
        None => {
            println!("Up to date (v{})", VERSION);
        }
    }
    Ok(())
}

fn test_tts() -> Result<()> {
    println!("Testing TTS (downloading piper if needed)...");

    let tts = tts::PiperTts::new()?;
    let player = audio::Player::new()?;

    for channel in channels::CHANNELS.iter() {
        println!("  Speaking: {}", channel.tts_name);
        player.speak(channel.tts_name, &tts);
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    player.speak("Radio off", &tts);
    std::thread::sleep(std::time::Duration::from_millis(500));

    println!("TTS test complete!");
    Ok(())
}

fn test_stream(url: &str) -> Result<()> {
    println!("Testing stream: {}", url);
    println!("Press Ctrl+C to stop");

    let mut player = audio::Player::new()?;
    player.play_stream(url)?;

    // Wait forever (until Ctrl+C)
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn install_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let service = r#"[Unit]
Description=LEGO Radio
After=network-online.target sound.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/lego-radio
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
"#;

        let path = "/etc/systemd/system/lego-radio.service";
        std::fs::write(path, service)?;
        println!("Created {}", path);

        // Run systemctl commands
        std::process::Command::new("systemctl")
            .args(["daemon-reload"])
            .status()?;

        std::process::Command::new("systemctl")
            .args(["enable", "lego-radio"])
            .status()?;

        std::process::Command::new("systemctl")
            .args(["start", "lego-radio"])
            .status()?;

        println!("Service installed and started!");
        println!("Check status: sudo systemctl status lego-radio");
    }

    #[cfg(not(target_os = "linux"))]
    {
        println!("Service installation only supported on Linux");
    }

    Ok(())
}

fn uninstall_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("systemctl")
            .args(["stop", "lego-radio"])
            .status()?;

        std::process::Command::new("systemctl")
            .args(["disable", "lego-radio"])
            .status()?;

        let path = "/etc/systemd/system/lego-radio.service";
        if std::path::Path::new(path).exists() {
            std::fs::remove_file(path)?;
            println!("Removed {}", path);
        }

        std::process::Command::new("systemctl")
            .args(["daemon-reload"])
            .status()?;

        println!("Service uninstalled!");
    }

    #[cfg(not(target_os = "linux"))]
    {
        println!("Service uninstallation only supported on Linux");
    }

    Ok(())
}
