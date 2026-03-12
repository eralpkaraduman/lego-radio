mod audio;
mod button;
mod channels;
mod metrics;
mod tts;
mod updater;

use anyhow::Result;
use button::ButtonEvent;
use log::{error, info, warn};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Sentry DSN for crash reporting and metrics
const SENTRY_DSN: &str = "https://a619cb8b77fe8255cce8eaab57f58108@o4511026110136320.ingest.de.sentry.io/4511027998228560";

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
    // Initialize Sentry for crash reporting (must be before logger)
    let _sentry_guard = sentry::init((
        SENTRY_DSN,
        sentry::ClientOptions {
            release: sentry::release_name!(),
            send_default_pii: true,
            ..Default::default()
        },
    ));

    // Initialize logger
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Set device context
    sentry::configure_scope(|scope| {
        scope.set_tag("platform", std::env::consts::OS);
        scope.set_tag("arch", std::env::consts::ARCH);
    });

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
            let url = args
                .get(2)
                .map(|s| s.as_str())
                .unwrap_or(channels::CHANNELS[0].url);
            return test_stream(url);
        }
        Some("--set-volume") => {
            let volume = args.get(2).and_then(|s| s.parse::<u8>().ok()).unwrap_or(80);
            return set_system_volume(volume);
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
    --version, -v       Print version
    --help, -h          Print this help
    --install           Install as systemd service
    --uninstall         Remove systemd service
    --update            Download and install latest version
    --set-volume <0-100> Set system audio volume (ALSA)
    --test-tts          Test text-to-speech
    --test-stream [URL] Test audio streaming

CONTROLS:
    Short press: Cycle to next channel
    Long press (2s): Jump to OFF state

    On Raspberry Pi: GPIO button on pin 17
    On Mac/Desktop:  Enter/Space (works like physical button)

Channels cycle: Welcome → 1 → 2 → ... → N → OFF → Welcome
"#,
        VERSION
    );
}

fn run_radio() -> Result<()> {
    info!("lego-radio v{} starting", VERSION);

    sentry::add_breadcrumb(sentry::Breadcrumb {
        category: Some("lifecycle".into()),
        message: Some(format!("lego-radio v{} starting", VERSION)),
        level: sentry::Level::Info,
        ..Default::default()
    });

    // Initialize TTS (downloads piper and voice model if needed)
    info!("Initializing TTS...");
    let tts = tts::PiperTts::new()?;
    sentry::add_breadcrumb(sentry::Breadcrumb {
        category: Some("init".into()),
        message: Some("TTS initialized".into()),
        level: sentry::Level::Info,
        ..Default::default()
    });

    // Simple audio pipeline (connect on demand)
    let mut pipeline = audio::AudioPipeline::new()?;

    // Start metrics collection thread
    let metrics_stop = Arc::new(AtomicBool::new(false));
    metrics::start_metrics_thread(metrics_stop.clone());

    // Channel for button events (input thread -> main thread)
    let (tx, rx) = std::sync::mpsc::channel::<button::ButtonEvent>();

    // Button input in background thread
    std::thread::spawn(move || {
        let button = button::create_button();
        if !button.is_gpio() {
            info!("(Enter/Space = button, hold 2s for off)");
        }
        loop {
            button.wait_for_press(&tx);
        }
    });

    // State machine starts at Welcome
    let mut state = RadioState::Welcome;
    let num_channels = channels::CHANNELS.len();

    // Handle Welcome state on startup
    handle_welcome(&mut pipeline, &tts);

    // Drain any button presses that happened during welcome
    while rx.try_recv().is_ok() {}

    let mut skip_wait = false; // Skip waiting for button press (after interrupt)

    // Debounce tracking
    let mut last_press_time = std::time::Instant::now() - std::time::Duration::from_secs(10);

    loop {
        if !skip_wait {
            // Wait for button down event
            let event = rx.recv().unwrap_or(ButtonEvent::Short);

            // Debounce: ignore if too soon after last press
            if last_press_time.elapsed() < std::time::Duration::from_millis(DEBOUNCE_MS) {
                // Drain any pending events and continue waiting
                while rx.try_recv().is_ok() {}
                continue;
            }

            last_press_time = std::time::Instant::now();

            // If we got a Down event, handle button held state
            if event == ButtonEvent::Down {
                // Stop everything immediately
                pipeline.stop();

                // Immediately advance state (skip behavior on button down)
                let pending_state = state.next(num_channels);
                info!("Button down - advancing to {:?}", pending_state);

                // Handle button down with continuous beep
                let final_event = handle_button_down(&mut pipeline, &rx);

                // Track button press
                let is_long = final_event == ButtonEvent::Long;
                sentry::add_breadcrumb(sentry::Breadcrumb {
                    category: Some("input".into()),
                    message: Some(format!(
                        "Button {} press",
                        if is_long { "long" } else { "short" }
                    )),
                    level: sentry::Level::Info,
                    ..Default::default()
                });

                // Handle long press: override to Off state
                if is_long {
                    info!("Long press detected - jumping to Off");
                    state = RadioState::Off;
                } else {
                    // Short press: use the pending state we calculated
                    state = pending_state;
                }
            } else {
                // Got Short or Long directly (shouldn't happen normally, but handle it)
                pipeline.stop();
                if event == ButtonEvent::Long {
                    state = RadioState::Off;
                } else {
                    pipeline.confirm_beep();
                    state = state.next(num_channels);
                }
            }
        }
        skip_wait = false; // Reset flag

        info!("State: {:?}", state);

        // Add breadcrumb for state transition
        sentry::add_breadcrumb(sentry::Breadcrumb {
            category: Some("state".into()),
            message: Some(format!("State: {:?}", state)),
            level: sentry::Level::Info,
            ..Default::default()
        });

        match state {
            RadioState::Welcome => {
                handle_welcome(&mut pipeline, &tts);
                // Drain any presses during welcome
                while rx.try_recv().is_ok() {}
            }
            RadioState::Playing(idx) => {
                // Play channel with interrupt support
                if let Some(event) = play_channel(&mut pipeline, &tts, idx, &rx) {
                    // Interrupted by button - handle the event
                    pipeline.stop();

                    let final_event = if event == ButtonEvent::Down {
                        // Button just pressed - handle with continuous beep
                        handle_button_down(&mut pipeline, &rx)
                    } else {
                        // Got Short or Long directly - play confirm for short
                        if event == ButtonEvent::Short {
                            pipeline.confirm_beep();
                        }
                        event
                    };

                    if final_event == ButtonEvent::Long {
                        state = RadioState::Off;
                    } else {
                        state = state.next(num_channels);
                    }

                    skip_wait = true;
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

/// Debounce duration - ignore presses within this window (milliseconds)
const DEBOUNCE_MS: u64 = 300;

/// How often to check stream status (milliseconds)
const STREAM_CHECK_INTERVAL_MS: u64 = 500;

/// Reconnection strategy with exponential backoff
const RECONNECT_INITIAL_SECS: u64 = 2; // Start with 2 second retry
const RECONNECT_MAX_SECS: u64 = 60; // Cap at 60 seconds
const RECONNECT_SILENT_RETRIES: u32 = 3; // Silent retries before announcing

/// Handle button down event: play continuous beep, wait for Short/Long
/// Returns the final event (Short or Long) indicating how the press ended
fn handle_button_down(
    pipeline: &mut audio::AudioPipeline,
    rx: &Receiver<ButtonEvent>,
) -> ButtonEvent {
    // Start continuous beep while button is held
    pipeline.start_beep();

    // Wait for final event (Short or Long)
    let final_event = loop {
        match rx.recv() {
            Ok(ButtonEvent::Short) => break ButtonEvent::Short,
            Ok(ButtonEvent::Long) => break ButtonEvent::Long,
            Ok(ButtonEvent::Down) => continue, // Ignore extra Down events
            Err(_) => break ButtonEvent::Short, // Default on error
        }
    };

    // Stop the beep
    pipeline.stop_beep();

    // Play confirmation chirp for short press (channel change feedback)
    if final_event == ButtonEvent::Short {
        pipeline.confirm_beep();
    }

    final_event
}

/// Play a channel with interrupt support and auto-reconnect
/// Returns None if completed normally, Some(event) if interrupted by button
fn play_channel(
    pipeline: &mut audio::AudioPipeline,
    tts: &tts::PiperTts,
    idx: usize,
    rx: &Receiver<ButtonEvent>,
) -> Option<ButtonEvent> {
    let channel = &channels::CHANNELS[idx];
    info!("Channel {}: {}", idx + 1, channel.name);

    sentry::add_breadcrumb(sentry::Breadcrumb {
        category: Some("playback".into()),
        message: Some(format!("Playing channel {}: {}", idx + 1, channel.name)),
        level: sentry::Level::Info,
        ..Default::default()
    });

    // Announce channel name (interruptible)
    if let Some(event) = pipeline.announce_interruptible(channel.tts_name, tts, Some(rx)) {
        info!("Interrupted during channel name");
        return Some(event);
    }

    // Announce connecting (interruptible)
    if let Some(event) = pipeline.announce_interruptible("Connecting", tts, Some(rx)) {
        info!("Interrupted during connecting");
        return Some(event);
    }

    // Main playback loop with auto-reconnect and exponential backoff
    let mut retry_count: u32 = 0;
    let mut retry_interval = RECONNECT_INITIAL_SECS;

    loop {
        // Try to connect
        if pipeline.connect_and_play(channel.url) {
            info!("Stream connected, monitoring playback");

            // Reset retry state on successful connection
            retry_count = 0;
            retry_interval = RECONNECT_INITIAL_SECS;

            // Monitor stream - check for button press or stream disconnect
            loop {
                // Check for button interrupt
                if let Ok(event) = rx.try_recv() {
                    info!("Interrupted by button press");
                    return Some(event);
                }

                // Check if stream is still active
                if !pipeline.is_stream_active() {
                    warn!("Stream disconnected, will reconnect");
                    sentry::add_breadcrumb(sentry::Breadcrumb {
                        category: Some("connection".into()),
                        message: Some(format!("Stream disconnected: {}", channel.name)),
                        level: sentry::Level::Warning,
                        ..Default::default()
                    });
                    break; // Exit monitor loop, will reconnect
                }

                std::thread::sleep(std::time::Duration::from_millis(STREAM_CHECK_INTERVAL_MS));
            }

            // Stream disconnected - silent retry first
            retry_count += 1;

            if retry_count > RECONNECT_SILENT_RETRIES {
                // Send Sentry event for persistent connection issues
                sentry::capture_message(
                    &format!(
                        "Stream reconnection required: {} (attempt {})",
                        channel.name, retry_count
                    ),
                    sentry::Level::Warning,
                );

                // Announce only after silent retries exhausted
                if let Some(event) =
                    pipeline.announce_interruptible("Connection lost. Reconnecting.", tts, Some(rx))
                {
                    info!("Interrupted during reconnect announcement");
                    return Some(event);
                }
            } else {
                info!(
                    "Silent reconnect attempt {} of {}",
                    retry_count, RECONNECT_SILENT_RETRIES
                );
            }
        } else {
            // Connection failed
            retry_count += 1;

            if retry_count > RECONNECT_SILENT_RETRIES {
                // Send Sentry event for connection failures
                sentry::capture_message(
                    &format!(
                        "Stream connection failed: {} (attempt {})",
                        channel.name, retry_count
                    ),
                    sentry::Level::Warning,
                );

                error!(
                    "Connection failed for {} (attempt {})",
                    channel.name, retry_count
                );
                if let Some(event) =
                    pipeline.announce_interruptible("Connection failed. Retrying.", tts, Some(rx))
                {
                    info!("Interrupted during retry announcement");
                    return Some(event);
                }
            } else {
                info!(
                    "Silent retry {} of {} for {}",
                    retry_count, RECONNECT_SILENT_RETRIES, channel.name
                );
            }
        }

        // Wait before retry with exponential backoff, checking for interrupts
        info!("Waiting {}s before reconnect attempt", retry_interval);
        let wait_until = std::time::Instant::now() + std::time::Duration::from_secs(retry_interval);

        while std::time::Instant::now() < wait_until {
            if let Ok(event) = rx.try_recv() {
                info!("Interrupted during reconnect wait");
                return Some(event);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // Exponential backoff: double the interval, cap at max
        retry_interval = (retry_interval * 2).min(RECONNECT_MAX_SECS);
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

            sentry::add_breadcrumb(sentry::Breadcrumb {
                category: Some("update".into()),
                message: Some(format!("Update available: v{}", version)),
                level: sentry::Level::Info,
                ..Default::default()
            });

            pipeline.announce("Update found. Installing.", tts);

            match updater::do_update_to(Some(&version)) {
                Ok(()) => {
                    sentry::capture_message(
                        &format!("Update successful: v{} -> v{}", VERSION, version),
                        sentry::Level::Info,
                    );
                    pipeline.announce("Update complete. Restarting.", tts);
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    std::process::exit(0);
                }
                Err(e) => {
                    error!("Update failed: {}", e);
                    sentry::integrations::anyhow::capture_anyhow(&e);
                    pipeline.announce("Update failed.", tts);
                }
            }
        }
        None => {
            info!("No updates available");
            pipeline.announce("Up to date.", tts);
        }
    }

    pipeline.announce("Ready.", tts);
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

fn set_system_volume(volume: u8) -> Result<()> {
    let volume = volume.min(100);
    println!("Setting system volume to {}%", volume);

    #[cfg(target_os = "linux")]
    {
        // Try amixer (ALSA) first
        let result = std::process::Command::new("amixer")
            .args(["sset", "Master", &format!("{}%", volume)])
            .output();

        match result {
            Ok(output) if output.status.success() => {
                println!("Volume set successfully (ALSA Master)");
                return Ok(());
            }
            _ => {
                // Try Digital control for HiFiBerry DAC
                let result = std::process::Command::new("amixer")
                    .args(["sset", "Digital", &format!("{}%", volume)])
                    .output();

                match result {
                    Ok(output) if output.status.success() => {
                        println!("Volume set successfully (ALSA Digital)");
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }

        // Try pactl (PipeWire/PulseAudio)
        let result = std::process::Command::new("pactl")
            .args(["set-sink-volume", "@DEFAULT_SINK@", &format!("{}%", volume)])
            .output();

        match result {
            Ok(output) if output.status.success() => {
                println!("Volume set successfully (PipeWire/PulseAudio)");
                Ok(())
            }
            _ => Err(anyhow::anyhow!(
                "Failed to set volume. No working audio control found."
            )),
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        println!("Volume control only supported on Linux");
        Ok(())
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
