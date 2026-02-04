mod audio;
mod button;
mod channels;
mod tts;
mod updater;

use anyhow::Result;
use log::{error, info};
use std::sync::mpsc::Receiver;

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

    // Initialize TTS (downloads piper and voice model if needed)
    info!("Initializing TTS...");
    let tts = tts::PiperTts::new()?;

    // Simple audio pipeline (connect on demand)
    let mut pipeline = audio::AudioPipeline::new()?;

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

    // Handle Welcome state on startup
    handle_welcome(&mut pipeline, &tts);

    // Drain any button presses that happened during welcome
    while rx.try_recv().is_ok() {}

    let mut skip_wait = false;  // Skip waiting for button press (after interrupt)

    loop {
        if !skip_wait {
            // Wait for button press
            rx.recv().ok();

            // Drain any extra queued button presses (rapid pressing)
            while rx.try_recv().is_ok() {}

            // Stop everything and clear buffer immediately
            pipeline.stop();

            // Immediate audio feedback
            pipeline.beep();

            // Transition to next state
            state = state.next(num_channels);
        }
        skip_wait = false;  // Reset flag

        info!("State: {:?}", state);

        match state {
            RadioState::Welcome => {
                handle_welcome(&mut pipeline, &tts);
                // Drain any presses during welcome
                while rx.try_recv().is_ok() {}
            }
            RadioState::Playing(idx) => {
                // Play channel with interrupt support
                if !play_channel(&mut pipeline, &tts, idx, &rx) {
                    // Interrupted - advance to next state and process immediately
                    state = state.next(num_channels);
                    pipeline.stop();
                    pipeline.beep();
                    skip_wait = true;  // Don't wait, process new state now
                    continue;
                }
            }
            RadioState::Off => {
                info!("Radio OFF");
                pipeline.announce("Radio off", &tts);
                // Drain any presses during announcement
                while rx.try_recv().is_ok() {}
            }
        }
    }
}

/// Play a channel with interrupt support
/// Returns false if interrupted (caller should continue loop)
fn play_channel(
    pipeline: &mut audio::AudioPipeline,
    tts: &tts::PiperTts,
    idx: usize,
    rx: &Receiver<()>,
) -> bool {
    let channel = &channels::CHANNELS[idx];
    info!("Channel {}: {}", idx + 1, channel.name);

    // Announce channel name (interruptible)
    if !pipeline.announce_interruptible(channel.tts_name, tts, Some(rx)) {
        info!("Interrupted during channel name");
        return false;
    }

    // Announce connecting (interruptible)
    if !pipeline.announce_interruptible("Connecting", tts, Some(rx)) {
        info!("Interrupted during connecting");
        return false;
    }

    // Connect and play
    if pipeline.connect_and_play(channel.url) {
        // Successfully connected - stream is now playing
        true
    } else {
        // Connection failed
        pipeline.announce("Station unavailable. Try another channel.", tts);
        true
    }
}

/// Handle the Welcome state - greet, check for updates
fn handle_welcome(pipeline: &mut audio::AudioPipeline, tts: &tts::PiperTts) {
    info!("Welcome - checking for updates");
    pipeline.announce("Hello!", tts);
    pipeline.announce("Checking for updates.", tts);

    match updater::check_for_update() {
        Some(version) => {
            info!("Update available: v{}", version);
            pipeline.announce("Update found. Installing.", tts);

            match updater::do_update() {
                Ok(()) => {
                    pipeline.announce("Update complete. Restarting.", tts);
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    std::process::exit(0);
                }
                Err(e) => {
                    error!("Update failed: {}", e);
                    pipeline.announce("Update failed.", tts);
                }
            }
        }
        None => {
            info!("No updates available");
            pipeline.announce("Up to date.", tts);
        }
    }

    pipeline.announce("Press button to select channel.", tts);
}

fn test_tts() -> Result<()> {
    println!("Testing TTS (downloading piper if needed)...");

    let tts = tts::PiperTts::new()?;
    let mut pipeline = audio::AudioPipeline::new()?;

    for channel in channels::CHANNELS.iter() {
        println!("  Speaking: {}", channel.tts_name);
        pipeline.announce(channel.tts_name, &tts);
    }

    pipeline.announce("Radio off", &tts);

    println!("TTS test complete!");
    Ok(())
}

fn test_stream(url: &str) -> Result<()> {
    println!("Testing stream: {}", url);
    println!("Press Ctrl+C to stop");

    let mut pipeline = audio::AudioPipeline::new()?;

    if pipeline.connect_and_play(url) {
        // Wait forever (until Ctrl+C)
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    } else {
        println!("Failed to connect to stream");
    }

    Ok(())
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
