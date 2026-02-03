mod audio;
mod button;
mod channels;
mod tts;
mod updater;

use anyhow::Result;
use log::{error, info};

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
    --install         Install as systemd service
    --uninstall       Remove systemd service
    --update          Download and install latest version
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

    // Initialize TTS (downloads piper and voice model if needed, checks capability once)
    info!("Initializing TTS...");
    let tts = std::sync::Arc::new(tts::PiperTts::new()?);

    // Audio player
    let mut player = audio::Player::new()?;

    // Channel for button events (input thread -> main thread)
    let (tx, rx) = std::sync::mpsc::channel::<()>();

    // Button input in background thread
    std::thread::spawn(move || {
        let button = button::create_button();
        if !button.is_gpio() {
            info!("(Press Enter to cycle channels)");
        }
        loop {
            button.wait_for_press();
            let _ = tx.send(());
        }
    });

    // Current channel index (incremented before use)
    // 0 = welcome/update channel (virtual)
    // 1+ = actual radio channels
    // After last channel = off, next press restarts from welcome
    let mut channel_idx: i32 = -1; // Will become 0 (welcome) on first iteration
    let mut pending_press = true; // Start with welcome sequence

    loop {
        // Check for button press (non-blocking if stream is playing)
        if !pending_press {
            // Block waiting for next press
            if rx.recv().is_ok() {
                pending_press = true;
            }
        }

        if !pending_press {
            continue;
        }

        // Consume any extra presses that happened during processing
        while rx.try_recv().is_ok() {}

        pending_press = false;
        channel_idx += 1;

        // Total channels = welcome (1) + radio channels + off state
        let num_radio_channels = channels::CHANNELS.len() as i32;

        if channel_idx == 0 || channel_idx > num_radio_channels + 1 {
            // Welcome channel - greet and check for updates (blocking TTS)
            channel_idx = 0;
            info!("Welcome channel - checking for updates");
            player.speak_sync("Hello!", &tts);
            player.speak_sync("Checking for updates. Please wait.", &tts);

            match updater::check_for_update() {
                Some(version) => {
                    info!("Update available: v{}", version);
                    player.speak_sync("Update found. Installing. This may take a minute.", &tts);

                    match updater::do_update() {
                        Ok(()) => {
                            player.speak_sync("Update complete. Restarting now.", &tts);
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            std::process::exit(0);
                        }
                        Err(e) => {
                            error!("Update failed: {}", e);
                            player.speak_sync("Update failed. Continuing anyway.", &tts);
                        }
                    }
                }
                None => {
                    info!("No updates available");
                    player.speak_sync("Up to date.", &tts);
                }
            }

            // Stay on welcome channel - user must press to start first radio channel
            player.speak_sync("Press button to start radio.", &tts);
        } else if channel_idx > num_radio_channels {
            // Past last channel - turn off
            info!("Radio OFF");
            player.stop();
            player.speak_sync("Radio off", &tts);
        } else {
            // Regular radio channel - fire-and-forget TTS, stream starts immediately
            let channel = &channels::CHANNELS[(channel_idx - 1) as usize];
            info!("Channel {}: {}", channel_idx, channel.name);

            player.stop();
            player.speak(channel.tts_name, &tts);

            if let Err(e) = player.play_stream(channel.url) {
                error!("Failed to play stream: {}", e);
                player.speak_sync("Stream error", &tts);
            }
        }
    }
}

fn test_tts() -> Result<()> {
    println!("Testing TTS (downloading piper if needed)...");

    let tts = std::sync::Arc::new(tts::PiperTts::new()?);
    let mut player = audio::Player::new()?;

    for channel in channels::CHANNELS.iter() {
        println!("  Speaking: {}", channel.tts_name);
        player.speak(channel.tts_name, &tts);
        // Wait for TTS to complete (fire-and-forget spawns thread)
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    player.speak("Radio off", &tts);
    std::thread::sleep(std::time::Duration::from_secs(2));

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
