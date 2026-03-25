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

/// Radio state machine (Welcome is handled at startup before this runs)
///
/// Playing: stream at 100%. Short press → Browsing.
/// Browsing: stream ducked to 20%, TTS announces channels. Commit on TTS end + 0.5s grace.
/// Off: no stream. Short press → Browsing. Long press from any state → Off.
#[derive(Debug, Clone, PartialEq)]
enum RadioState {
    /// Streaming a channel at full volume.
    Playing(usize),
    /// Browsing channels while old stream keeps playing (ducked).
    /// `playing`: currently streaming channel (None if coming from Off)
    /// `browse`: channel index being announced via TTS
    Browsing {
        playing: Option<usize>,
        browse: usize,
    },
    /// Radio off. No stream.
    Off,
}

impl RadioState {
    /// Handle short press: enter or advance browsing
    fn short_press(self, num_channels: usize) -> RadioState {
        match self {
            RadioState::Playing(ch) => RadioState::Browsing {
                playing: Some(ch),
                browse: (ch + 1) % num_channels,
            },
            RadioState::Browsing { playing, browse } => RadioState::Browsing {
                playing,
                browse: (browse + 1) % num_channels,
            },
            RadioState::Off => RadioState::Browsing {
                playing: None,
                browse: 0,
            },
        }
    }

    /// Commit the browsed channel (TTS finished + grace period elapsed)
    fn commit(self) -> RadioState {
        match self {
            RadioState::Browsing { browse, .. } => RadioState::Playing(browse),
            other => other,
        }
    }

    /// Long press: always go to Off
    fn long_press(self) -> RadioState {
        RadioState::Off
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
    // On macOS, GUI event loop must run on the main thread.
    // Radio logic runs on a background thread.
    #[cfg(not(target_os = "linux"))]
    {
        let (button, run_gui) = button::create_gui_button();
        info!("Using GUI button window (click = short press, hold = long press)");

        std::thread::spawn(move || {
            if let Err(e) = run_radio_with_button(button) {
                error!("Radio error: {}", e);
                std::process::exit(1);
            }
        });

        run_gui(); // Blocks forever on main thread
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        let button = button::create_button();
        run_radio_with_button(button)
    }
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
    On Mac/Desktop:  GUI window (click and hold = button press)

Channels cycle: Welcome → 1 → 2 → ... → N → OFF → Welcome
"#,
        VERSION
    );
}

fn run_radio_with_button(button: Box<dyn button::ButtonInput>) -> Result<()> {
    info!("lego-radio v{} starting", VERSION);

    sentry::add_breadcrumb(sentry::Breadcrumb {
        category: Some("lifecycle".into()),
        message: Some(format!("lego-radio v{} starting", VERSION)),
        level: sentry::Level::Info,
        ..Default::default()
    });

    info!("TTS: using embedded audio");

    let mut pipeline = audio::AudioPipeline::new()?;

    // Start metrics collection thread
    let metrics_stop = Arc::new(AtomicBool::new(false));
    metrics::start_metrics_thread(metrics_stop.clone());

    // Channel for button events (input thread -> main thread)
    let (tx, rx) = std::sync::mpsc::channel::<button::ButtonEvent>();

    // Button input in background thread
    std::thread::spawn(move || loop {
        button.wait_for_press(&tx);
    });

    let num_channels = channels::CHANNELS.len();

    // Welcome state: blocking, no button input accepted
    handle_welcome(&mut pipeline);
    while rx.try_recv().is_ok() {} // drain

    // Auto-play first channel
    // Auto-play first channel
    let mut state = RadioState::Playing(0);
    info!("State: {:?}", state);
    let first_channel = &channels::CHANNELS[0];
    pipeline.announce(first_channel.tts_name);
    start_channel(&mut pipeline, 0);

    // Main event loop
    loop {
        match &state {
            RadioState::Playing(_) => {
                // Wait for button event or stream disconnect
                match wait_for_event(&pipeline, &rx) {
                    RadioEvent::ButtonDown => {
                        let press = handle_button_press(&mut pipeline, &rx);
                        if press == ButtonEvent::Long {
                            state = state.long_press();
                            handle_state_enter(&mut pipeline, &state);
                        } else {
                            state = state.short_press(num_channels);
                            handle_state_enter(&mut pipeline, &state);
                        }
                    }
                    RadioEvent::StreamEnded => {
                        handle_reconnect(&mut pipeline, &state, &rx);
                    }
                }
            }
            RadioState::Browsing { .. } => {
                // Wait for TTS to finish + grace period, or button press
                match wait_for_tts_or_button(&pipeline, &rx) {
                    BrowseEvent::ButtonDown => {
                        let press = handle_button_press(&mut pipeline, &rx);
                        if press == ButtonEvent::Long {
                            state = state.long_press();
                            handle_state_enter(&mut pipeline, &state);
                        } else {
                            state = state.short_press(num_channels);
                            handle_state_enter(&mut pipeline, &state);
                        }
                    }
                    BrowseEvent::Commit => {
                        // TTS finished + grace elapsed, commit the channel
                        let old_playing = if let RadioState::Browsing { playing, .. } = &state {
                            *playing
                        } else {
                            None
                        };
                        let new_state = state.commit();
                        if let RadioState::Playing(new_ch) = &new_state {
                            if old_playing == Some(*new_ch) {
                                // Same channel — just restore volume
                                info!("Commit: same channel, restoring volume");
                                pipeline.restore_stream();
                            } else {
                                // Different channel — stop old, connect new
                                info!("Commit: switching to channel {}", new_ch);
                                pipeline.stop_stream();
                                pipeline.restore_stream();
                                start_channel(&mut pipeline, *new_ch);
                            }
                        }
                        state = new_state;
                    }
                }
            }
            RadioState::Off => {
                // Wait for voice ("Radio off") to finish, interruptible
                while pipeline.is_voice_playing() {
                    if let Ok(ButtonEvent::Down) = rx.try_recv() {
                        pipeline.stop_voice();
                        let press = handle_button_press(&mut pipeline, &rx);
                        if press != ButtonEvent::Long {
                            // Turn back on: full welcome sequence
                            handle_welcome(&mut pipeline);
                            while rx.try_recv().is_ok() {}
                            state = RadioState::Playing(0);
                            let first_channel = &channels::CHANNELS[0];
                            pipeline.announce(first_channel.tts_name);
                            start_channel(&mut pipeline, 0);
                        }
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                // If still Off, wait for button press
                if state == RadioState::Off {
                    loop {
                        match rx.recv() {
                            Ok(ButtonEvent::Down) => {
                                let press = handle_button_press(&mut pipeline, &rx);
                                if press != ButtonEvent::Long {
                                    // Turn back on: full welcome sequence
                                    handle_welcome(&mut pipeline);
                                    while rx.try_recv().is_ok() {}
                                    state = RadioState::Playing(0);
                                    let first_channel = &channels::CHANNELS[0];
                                    pipeline.announce(first_channel.tts_name);
                                    start_channel(&mut pipeline, 0);
                                }
                                break;
                            }
                            Ok(_) => continue,
                            Err(_) => break,
                        }
                    }
                }
            }
        }

        info!("State: {:?}", state);
    }
}

/// Grace period after TTS finishes before committing channel (milliseconds)
const BROWSE_GRACE_MS: u64 = 500;

/// How often to check stream status (milliseconds)
const STREAM_CHECK_INTERVAL_MS: u64 = 500;

/// Reconnection strategy with exponential backoff
const RECONNECT_INITIAL_SECS: u64 = 2;
const RECONNECT_MAX_SECS: u64 = 60;
const RECONNECT_SILENT_RETRIES: u32 = 3;

/// Events that can happen while Playing
enum RadioEvent {
    ButtonDown,
    StreamEnded,
}

/// Events that can happen while Browsing
enum BrowseEvent {
    ButtonDown,
    Commit, // TTS finished + grace period elapsed
}

/// Handle a button Down event: play beep while held, return Short or Long
fn handle_button_press(
    pipeline: &mut audio::AudioPipeline,
    rx: &Receiver<ButtonEvent>,
) -> ButtonEvent {
    pipeline.start_beep();

    let final_event = loop {
        match rx.recv() {
            Ok(ButtonEvent::Short) => break ButtonEvent::Short,
            Ok(ButtonEvent::Long) => break ButtonEvent::Long,
            Ok(ButtonEvent::Down) => continue,
            Err(_) => break ButtonEvent::Short,
        }
    };

    pipeline.stop_beep();

    if final_event == ButtonEvent::Long {
        // Wait for button release — consume events until quiet for 300ms
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(300)) {
                Ok(_) => continue, // still getting events, button still held
                Err(_) => break,   // no events for 300ms, button released
            }
        }
    } else {
        pipeline.confirm_beep();
    }

    final_event
}

/// Wait for either a button press or stream disconnect
fn wait_for_event(pipeline: &audio::AudioPipeline, rx: &Receiver<ButtonEvent>) -> RadioEvent {
    loop {
        // Check for button press (non-blocking)
        if let Ok(ButtonEvent::Down) = rx.try_recv() {
            return RadioEvent::ButtonDown;
        }

        // Check stream health
        if !pipeline.is_stream_active() {
            return RadioEvent::StreamEnded;
        }

        std::thread::sleep(std::time::Duration::from_millis(STREAM_CHECK_INTERVAL_MS));
    }
}

/// Wait for TTS to finish + grace period, or a button press
fn wait_for_tts_or_button(
    pipeline: &audio::AudioPipeline,
    rx: &Receiver<ButtonEvent>,
) -> BrowseEvent {
    // Wait for voice to finish playing
    while pipeline.is_voice_playing() {
        if let Ok(ButtonEvent::Down) = rx.try_recv() {
            return BrowseEvent::ButtonDown;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Grace period
    let grace_end = std::time::Instant::now() + std::time::Duration::from_millis(BROWSE_GRACE_MS);
    while std::time::Instant::now() < grace_end {
        if let Ok(ButtonEvent::Down) = rx.try_recv() {
            return BrowseEvent::ButtonDown;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    BrowseEvent::Commit
}

/// Enter a new state: play appropriate audio feedback (non-blocking)
/// The main loop handles waiting for TTS / checking for button presses.
fn handle_state_enter(pipeline: &mut audio::AudioPipeline, state: &RadioState) {
    match state {
        RadioState::Browsing { playing, browse } => {
            if playing.is_some() {
                pipeline.duck_stream();
            }
            let channel = &channels::CHANNELS[*browse];
            info!("Browsing: {}", channel.name);
            pipeline.speak(channel.tts_name);
        }
        RadioState::Off => {
            pipeline.stop_stream();
            info!("Radio OFF");
            pipeline.speak("Radio off");
        }
        RadioState::Playing(idx) => {
            info!("Playing: {}", channels::CHANNELS[*idx].name);
        }
    }
}

/// Start playing a channel (connect stream)
fn start_channel(pipeline: &mut audio::AudioPipeline, idx: usize) {
    let channel = &channels::CHANNELS[idx];
    info!("Connecting to: {}", channel.name);

    sentry::add_breadcrumb(sentry::Breadcrumb {
        category: Some("playback".into()),
        message: Some(format!("Playing channel {}: {}", idx + 1, channel.name)),
        level: sentry::Level::Info,
        ..Default::default()
    });

    if pipeline.connect_and_play(channel.url) {
        pipeline.fade_in_stream();
    } else {
        warn!("Failed to connect to {}", channel.name);
    }
}

/// Handle stream reconnection with exponential backoff
fn handle_reconnect(
    pipeline: &mut audio::AudioPipeline,
    state: &RadioState,
    rx: &Receiver<ButtonEvent>,
) {
    let idx = match state {
        RadioState::Playing(i) => *i,
        _ => return,
    };

    let channel = &channels::CHANNELS[idx];
    let mut retry_count: u32 = 0;
    let mut retry_interval = RECONNECT_INITIAL_SECS;

    loop {
        retry_count += 1;
        if retry_count > RECONNECT_SILENT_RETRIES {
            warn!("Reconnecting to {} (attempt {})", channel.name, retry_count);
            if let Some(ButtonEvent::Down) =
                pipeline.announce_interruptible("Connection lost. Reconnecting.", Some(rx))
            {
                return; // interrupted by button, main loop will handle
            }
        } else {
            info!(
                "Silent reconnect {} of {} for {}",
                retry_count, RECONNECT_SILENT_RETRIES, channel.name
            );
        }

        // Wait before retry, checking for button interrupts
        let wait_until = std::time::Instant::now() + std::time::Duration::from_secs(retry_interval);
        while std::time::Instant::now() < wait_until {
            if let Ok(ButtonEvent::Down) = rx.try_recv() {
                return; // interrupted
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        if pipeline.connect_and_play(channel.url) {
            info!("Reconnected to {}", channel.name);
            return;
        }

        retry_interval = (retry_interval * 2).min(RECONNECT_MAX_SECS);
    }
}

/// Handle the Welcome state - greet, check for updates
fn handle_welcome(pipeline: &mut audio::AudioPipeline) {
    info!("Welcome - checking for updates");
    pipeline.announce("Hello!");
    pipeline.announce("Checking for updates.");

    match updater::check_for_update() {
        Some(version) => {
            info!("Update available: v{}", version);

            sentry::add_breadcrumb(sentry::Breadcrumb {
                category: Some("update".into()),
                message: Some(format!("Update available: v{}", version)),
                level: sentry::Level::Info,
                ..Default::default()
            });

            pipeline.announce("Update found. Installing.");

            match updater::do_update_to(Some(&version)) {
                Ok(()) => {
                    sentry::capture_message(
                        &format!("Update successful: v{} -> v{}", VERSION, version),
                        sentry::Level::Info,
                    );
                    pipeline.announce("Update complete. Restarting.");
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    std::process::exit(0);
                }
                Err(e) => {
                    error!("Update failed: {}", e);
                    sentry::integrations::anyhow::capture_anyhow(&e);
                    pipeline.announce("Update failed.");
                }
            }
        }
        None => {
            info!("No updates available");
            pipeline.announce("Up to date.");
        }
    }
}

fn test_tts() -> Result<()> {
    println!("Testing embedded TTS audio...");

    let pipeline = audio::AudioPipeline::new()?;

    for channel in channels::CHANNELS.iter() {
        println!("  Speaking: {}", channel.tts_name);
        pipeline.announce(channel.tts_name);
    }

    pipeline.announce("Radio off");

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

    const N: usize = 4; // test with 4 channels

    // ==================== Short press: Playing → Browsing ====================

    #[test]
    fn test_playing_short_press_enters_browsing() {
        let state = RadioState::Playing(0);
        assert_eq!(
            state.short_press(N),
            RadioState::Browsing {
                playing: Some(0),
                browse: 1
            }
        );
    }

    #[test]
    fn test_playing_last_channel_short_press_wraps_to_zero() {
        let state = RadioState::Playing(3);
        assert_eq!(
            state.short_press(N),
            RadioState::Browsing {
                playing: Some(3),
                browse: 0
            }
        );
    }

    // ==================== Short press: Browsing advances ====================

    #[test]
    fn test_browsing_short_press_advances() {
        let state = RadioState::Browsing {
            playing: Some(0),
            browse: 1,
        };
        assert_eq!(
            state.short_press(N),
            RadioState::Browsing {
                playing: Some(0),
                browse: 2
            }
        );
    }

    #[test]
    fn test_browsing_short_press_wraps_around() {
        let state = RadioState::Browsing {
            playing: Some(0),
            browse: 3,
        };
        assert_eq!(
            state.short_press(N),
            RadioState::Browsing {
                playing: Some(0),
                browse: 0
            }
        );
    }

    #[test]
    fn test_browsing_preserves_playing_channel() {
        // Playing channel 2, browsing through channels
        let mut state = RadioState::Browsing {
            playing: Some(2),
            browse: 0,
        };
        state = state.short_press(N); // browse → 1
        state = state.short_press(N); // browse → 2
        state = state.short_press(N); // browse → 3
        assert_eq!(
            state,
            RadioState::Browsing {
                playing: Some(2),
                browse: 3
            }
        );
    }

    // ==================== Short press: Off → Browsing ====================

    #[test]
    fn test_off_short_press_enters_browsing_at_zero() {
        let state = RadioState::Off;
        assert_eq!(
            state.short_press(N),
            RadioState::Browsing {
                playing: None,
                browse: 0
            }
        );
    }

    // ==================== Commit ====================

    #[test]
    fn test_commit_switches_to_playing() {
        let state = RadioState::Browsing {
            playing: Some(0),
            browse: 2,
        };
        assert_eq!(state.commit(), RadioState::Playing(2));
    }

    #[test]
    fn test_commit_same_channel_stays_playing() {
        let state = RadioState::Browsing {
            playing: Some(1),
            browse: 1,
        };
        assert_eq!(state.commit(), RadioState::Playing(1));
    }

    #[test]
    fn test_commit_from_off_starts_playing() {
        let state = RadioState::Browsing {
            playing: None,
            browse: 3,
        };
        assert_eq!(state.commit(), RadioState::Playing(3));
    }

    #[test]
    fn test_commit_non_browsing_is_noop() {
        assert_eq!(RadioState::Playing(0).commit(), RadioState::Playing(0));
        assert_eq!(RadioState::Off.commit(), RadioState::Off);
    }

    // ==================== Long press ====================

    #[test]
    fn test_long_press_from_playing() {
        assert_eq!(RadioState::Playing(2).long_press(), RadioState::Off);
    }

    #[test]
    fn test_long_press_from_browsing() {
        let state = RadioState::Browsing {
            playing: Some(0),
            browse: 2,
        };
        assert_eq!(state.long_press(), RadioState::Off);
    }

    #[test]
    fn test_long_press_from_off() {
        assert_eq!(RadioState::Off.long_press(), RadioState::Off);
    }

    // ==================== Full scenarios ====================

    #[test]
    fn test_scenario_browse_and_commit() {
        // Playing ch0, browse to ch2, commit
        let mut state = RadioState::Playing(0);
        state = state.short_press(N); // Browsing { playing: 0, browse: 1 }
        state = state.short_press(N); // Browsing { playing: 0, browse: 2 }
        state = state.commit(); // Playing(2)
        assert_eq!(state, RadioState::Playing(2));
    }

    #[test]
    fn test_scenario_browse_full_circle_back_to_same() {
        // Playing ch1, browse all the way around back to ch1, commit
        let mut state = RadioState::Playing(1);
        state = state.short_press(N); // browse: 2
        state = state.short_press(N); // browse: 3
        state = state.short_press(N); // browse: 0
        state = state.short_press(N); // browse: 1 (back to playing)
        assert_eq!(
            state,
            RadioState::Browsing {
                playing: Some(1),
                browse: 1
            }
        );
        state = state.commit();
        assert_eq!(state, RadioState::Playing(1)); // same channel
    }

    #[test]
    fn test_scenario_off_browse_commit() {
        // From Off, browse to ch2, commit
        let mut state = RadioState::Off;
        state = state.short_press(N); // Browsing { playing: None, browse: 0 }
        state = state.short_press(N); // browse: 1
        state = state.short_press(N); // browse: 2
        state = state.commit();
        assert_eq!(state, RadioState::Playing(2));
    }

    #[test]
    fn test_scenario_browse_then_long_press() {
        // Playing ch0, start browsing, then long press → Off
        let mut state = RadioState::Playing(0);
        state = state.short_press(N); // Browsing
        state = state.short_press(N); // advance browse
        state = state.long_press(); // Off
        assert_eq!(state, RadioState::Off);
    }

    #[test]
    fn test_scenario_single_channel_wraps() {
        // Only 1 channel: browse wraps to itself
        let state = RadioState::Playing(0);
        let state = state.short_press(1); // browse: (0+1)%1 = 0
        assert_eq!(
            state,
            RadioState::Browsing {
                playing: Some(0),
                browse: 0
            }
        );
    }
}
