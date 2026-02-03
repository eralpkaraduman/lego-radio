mod audio;
mod button;
mod channels;
mod tts;
mod updater;

use anyhow::Result;
use log::{error, info};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Radio state machine - explicit states instead of magic index numbers
#[derive(Debug, Clone, Copy, PartialEq)]
enum RadioState {
    Welcome,
    Playing(usize), // channel index 0..N-1
    Off,
}

impl RadioState {
    /// Transition to next state on button press
    fn next(self, num_channels: usize) -> RadioState {
        match self {
            RadioState::Welcome => RadioState::Playing(0),
            RadioState::Playing(i) if i + 1 < num_channels => RadioState::Playing(i + 1),
            RadioState::Playing(_) => RadioState::Off,
            RadioState::Off => RadioState::Welcome,
        }
    }
}

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

Channels cycle: Welcome → 1 → 2 → 3 → 4 → OFF → Welcome
"#,
        VERSION
    );
}

fn run_radio() -> Result<()> {
    info!("lego-radio v{} starting", VERSION);

    // Initialize TTS (downloads piper and voice model if needed, checks capability once)
    info!("Initializing TTS...");
    let tts = std::sync::Arc::new(tts::PiperTts::new()?);

    // Multi-stream audio player (all channels playing, one audible)
    let mut player = audio::MultiStreamPlayer::new()?;

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

    // State machine starts at Welcome
    let mut state = RadioState::Welcome;
    let num_channels = channels::CHANNELS.len();

    // Handle Welcome state on startup (connects all streams)
    handle_welcome(&mut player, &tts);

    loop {
        // Wait for button press
        rx.recv().ok();

        // Consume any extra presses that happened during processing
        let mut discarded = 0;
        while rx.try_recv().is_ok() {
            discarded += 1;
        }
        if discarded > 0 {
            log::debug!("Discarded {} extra button press(es)", discarded);
        }

        // Transition to next state
        state = state.next(num_channels);
        info!("State: {:?}", state);

        match state {
            RadioState::Welcome => {
                handle_welcome(&mut player, &tts);
            }
            RadioState::Playing(idx) => {
                let channel = &channels::CHANNELS[idx];
                info!("Channel {}: {}", idx + 1, channel.name);

                // Fire-and-forget TTS (ducks all streams), then instant switch
                player.speak(channel.tts_name, &tts);
                player.select(idx);  // INSTANT - just volume change

                // Check if stream is in error state and needs reconnection
                if player.active_stream_has_error() {
                    player.speak_sync("Reconnecting.", &tts);

                    if player.reconnect_active_with_backoff(channel) {
                        player.speak_sync("Connected.", &tts);
                    } else {
                        player.speak_sync("Station unavailable.", &tts);
                    }
                }
            }
            RadioState::Off => {
                info!("Radio OFF");
                player.speak_sync("Radio off", &tts);
                player.disconnect_all();  // Save bandwidth
            }
        }
    }
}

/// Handle the Welcome state - greet, check for updates, connect all streams
fn handle_welcome(player: &mut audio::MultiStreamPlayer, tts: &std::sync::Arc<tts::PiperTts>) {
    info!("Welcome - checking for updates");
    player.speak_sync("Hello!", tts);
    player.speak_sync("Checking for updates. Please wait.", tts);

    match updater::check_for_update() {
        Some(version) => {
            info!("Update available: v{}", version);
            player.speak_sync("Update found. Installing. This may take a minute.", tts);

            match updater::do_update() {
                Ok(()) => {
                    player.speak_sync("Update complete. Restarting now.", tts);
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    std::process::exit(0);
                }
                Err(e) => {
                    error!("Update failed: {}", e);
                    player.speak_sync("Update failed. Continuing anyway.", tts);
                }
            }
        }
        None => {
            info!("No updates available");
            player.speak_sync("Up to date.", tts);
        }
    }

    // Connect all streams
    player.speak_sync("Connecting to stations.", tts);
    let connected = player.connect_all(
        channels::CHANNELS,
        std::time::Duration::from_secs(10),
    );

    // Announce connection status
    let msg = format!(
        "Connected {} out of {} stations.",
        connected,
        channels::CHANNELS.len()
    );
    player.speak_sync(&msg, tts);

    player.speak_sync("Change channel to start playing.", tts);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_welcome_to_playing() {
        let state = RadioState::Welcome;
        assert_eq!(state.next(4), RadioState::Playing(0));
    }

    #[test]
    fn test_state_playing_next_channel() {
        let state = RadioState::Playing(0);
        assert_eq!(state.next(4), RadioState::Playing(1));

        let state = RadioState::Playing(2);
        assert_eq!(state.next(4), RadioState::Playing(3));
    }

    #[test]
    fn test_state_playing_last_to_off() {
        let state = RadioState::Playing(3);
        assert_eq!(state.next(4), RadioState::Off);
    }

    #[test]
    fn test_state_off_to_welcome() {
        let state = RadioState::Off;
        assert_eq!(state.next(4), RadioState::Welcome);
    }

    #[test]
    fn test_state_full_cycle() {
        let mut state = RadioState::Welcome;
        let n = 4;

        state = state.next(n); // -> Playing(0)
        assert_eq!(state, RadioState::Playing(0));

        state = state.next(n); // -> Playing(1)
        state = state.next(n); // -> Playing(2)
        state = state.next(n); // -> Playing(3)
        assert_eq!(state, RadioState::Playing(3));

        state = state.next(n); // -> Off
        assert_eq!(state, RadioState::Off);

        state = state.next(n); // -> Welcome
        assert_eq!(state, RadioState::Welcome);
    }

    #[test]
    fn test_state_single_channel() {
        // Edge case: only 1 channel
        let state = RadioState::Welcome;
        assert_eq!(state.next(1), RadioState::Playing(0));

        let state = RadioState::Playing(0);
        assert_eq!(state.next(1), RadioState::Off);
    }
}
