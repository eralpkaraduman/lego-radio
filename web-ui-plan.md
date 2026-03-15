# Web UI with SSE for LEGO Radio

## Context

The radio currently has no remote control — volume is hardcoded at 1.0, channels cycle via physical button only. We're adding a web interface served from the Rust binary on port 8080, using **Server-Sent Events (SSE)** for real-time server→client state push and **POST** endpoints for client→server commands. No JS frameworks, no async runtime — just `tiny-http`, vanilla HTML/JS, and std threads. Target: Raspberry Pi 4B / RaspberryOS.

## UI Layout

Mobile-only vertical layout. All controls full-width, large touch targets.

```
┌──────────────────────────┐
│ Eralp's LEGO Radio       │
│                   v0.5.5 │
│                          │
│   ┌──────┐  ┌───────┐   │
│   │  ON  │  │  OFF  │   │
│   └──────┘  └───────┘   │
│                          │
│ Volume                   │
│ ━━━━━━━━━━━●━━━━━━  75% │
│                          │
│ Channels                 │
│ ┌──────────────────────┐ │
│ │ ● YLE Klassinen      │ │  ← active (highlighted)
│ │   YLE Radio 1        │ │
│ │   YLE Radio Suomi    │ │
│ │   YleX               │ │
│ │   Soma FM Groove...  │ │
│ │   ...                │ │
│ └──────────────────────┘ │
│                          │
│         [Update]         │
└──────────────────────────┘
```

- **On/Off**: large toggle buttons. "On" resumes last channel (or first). "Off" stops playback.
- **Volume slider**: full-width. On drag → optimistic UI update. On release (`change` event) → POST to server. SSE pushes ground truth but is suppressed during active drag.
- **Channel list**: tappable rows. Tapping a channel when off turns radio on to that channel. Active channel highlighted.
- **Update button**: triggers check + install + restart flow.
- **Version**: displayed from `VERSION` constant.

## Architecture

### Unified Command Channel

The codebase checks `rx.try_recv()` for button interrupts at **4 separate points**:
1. Main loop `rx.recv()` — `main.rs:191`
2. `play_channel()` stream monitor — `main.rs:387`
3. `play_channel()` reconnect wait — `main.rs:470`
4. `announce_interruptible()` — `audio.rs:318`

Rather than adding a second receiver at all 4 points, we introduce a unified `RadioCommand` enum that both button events and web commands flow through on a single channel:

```rust
// src/commands.rs
pub enum RadioCommand {
    // Physical button events (mapped from ButtonEvent)
    ButtonDown,
    ButtonShort,
    ButtonLong,
    // Web UI commands
    SetChannel(usize),
    TurnOff,
    TurnOn,
    SetVolume(u8),
    TriggerUpdate,
}
```

A bridge thread in `run_radio()` converts `ButtonEvent → RadioCommand`. The web server sends its commands on the same `Sender<RadioCommand>`. **Zero changes to button.rs.**

### Volume Handling

`SetVolume` must not interrupt playback. A helper function handles this:

```rust
/// Drains SetVolume commands (applies immediately), returns first non-volume command.
/// Replaces every rx.try_recv() in the codebase.
pub fn try_recv_command(rx: &Receiver<RadioCommand>, sink: &Arc<Sink>) -> Option<RadioCommand> {
    loop {
        match rx.try_recv() {
            Ok(RadioCommand::SetVolume(v)) => {
                sink.set_volume(v as f32 / 100.0);
            }
            Ok(cmd) => return Some(cmd),
            Err(_) => return None,
        }
    }
}
```

### Shared State

```rust
// src/web.rs
pub struct SharedState {
    pub power: AtomicBool,
    pub channel_index: AtomicI8,      // -1 = none, 0..N = index
    pub channel_name: Mutex<&'static str>,
    pub volume: AtomicU8,             // 0-100
    pub version: &'static str,
}
```

Wrapped in `Arc<SharedState>`, written by main loop on every state transition, read by SSE threads.

### Data Flow

```
Phone browser ──POST──→ tiny-http thread ──mpsc──→ main loop (state machine)
                                                       │
Phone browser ←──SSE───← SSE thread ←── reads ←── Arc<SharedState>

Physical button ──ButtonEvent──→ bridge thread ──RadioCommand──→ main loop
```

1. Main loop updates `SharedState` on every state transition
2. SSE thread reads `SharedState` every ~300ms, pushes JSON to connected browsers
3. POST handlers parse commands, send as `RadioCommand` via shared mpsc sender
4. Main loop processes all commands from the single unified channel

### SSE Implementation

Uses `os_pipe` crate to create a `(reader, writer)` pipe pair. The reader is handed to tiny-http's `Response::new()` with `None` content length (triggers chunked transfer). A pusher thread writes SSE-formatted data to the writer end.

```rust
fn handle_sse(request: tiny_http::Request, shared: &Arc<SharedState>) {
    let (pipe_reader, mut pipe_writer) = os_pipe::pipe().unwrap();

    // Respond with streaming SSE headers — tiny-http streams from pipe_reader
    let response = tiny_http::Response::new(
        tiny_http::StatusCode(200),
        vec![
            tiny_http::Header::from_bytes("Content-Type", "text/event-stream").unwrap(),
            tiny_http::Header::from_bytes("Cache-Control", "no-cache").unwrap(),
        ],
        pipe_reader,
        None, // No content length → chunked transfer
        None,
    );

    let shared = shared.clone();
    // Spawn pusher thread — writes SSE events to pipe, tiny-http reads from other end
    std::thread::spawn(move || {
        // respond() blocks this thread, streaming from pipe_reader as pusher writes
        let _ = request.respond(response);
    });

    std::thread::spawn(move || {
        loop {
            let json = format_state_json(&shared);
            if write!(pipe_writer, "data: {}\n\n", json).is_err() {
                break; // Client disconnected
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    });
}
```

No custom `Read` impl needed. The `os_pipe` crate provides the OS-level pipe; tiny-http reads from one end while we write SSE events to the other. Client disconnect causes a broken pipe error, cleanly exiting the pusher thread. Cap at 4 concurrent SSE connections.

JSON payload: `{"volume":75,"power":true,"channel":0,"channelName":"YLE Klassinen","version":"0.5.5"}`

### Slider Behavior

- `input` event → optimistic UI update (display changes immediately)
- `change` event (mouseup/touchend) → `POST /volume` with final value
- `touchstart`/`mousedown` sets `dragging` flag → SSE updates skip volume field while dragging
- `touchend`/`mouseup` clears flag → SSE resumes overriding volume display

### Silent Web Commands

When a channel is set via the web UI, skip TTS announcements ("Connecting...", channel name). Add `silent: bool` parameter to `play_channel()`. Physical button presses still announce as before.

## Files to Modify

### New: `src/commands.rs` (~40 lines)
- `RadioCommand` enum
- `try_recv_command()` helper

### New: `src/web.rs` (~300 lines)
- `SharedState` struct
- `pub fn start(shared: Arc<SharedState>, cmd_tx: Sender<RadioCommand>)` — spawns tiny-http server on `0.0.0.0:8080`
- HTTP routing: `GET /` (HTML), `GET /events` (SSE), `GET /channels` (JSON), `POST /volume`, `POST /power`, `POST /channel`, `POST /update`
- SSE via `os_pipe` — no custom `Read` impl needed
- Embedded HTML via `include_str!("web.html")` or inline const — mobile-only, inline CSS/JS

### Modify: `src/audio.rs`
- Remove `const VOLUME: f32 = 1.0` (line 19)
- `AudioPipeline::new()` takes `initial_volume: u8`, calls `sink.set_volume(v as f32 / 100.0)`
- Add `pub fn set_volume(&self, percent: u8)`
- Add `pub fn sink(&self) -> &Arc<Sink>` for use by `try_recv_command`
- Change `announce_interruptible()` to accept `&Receiver<RadioCommand>` instead of `&Receiver<ButtonEvent>`, handle `SetVolume` inline

### Modify: `src/main.rs`
- Add `mod web;` and `mod commands;`
- Create `Arc<SharedState>` and `mpsc::channel::<RadioCommand>()`
- Button bridge thread: `for event in btn_rx { cmd_tx.send(event.into()) }`
- Start web: `web::start(shared.clone(), cmd_tx.clone())`
- Replace blocking `rx.recv()` with `cmd_rx.recv_timeout(100ms)` + volume drain loop
- Replace all `rx.try_recv()` with `commands::try_recv_command(&cmd_rx, pipeline.sink())`
- Handle web commands in main match:
  - `SetChannel(idx)` → stop, set `state = Playing(idx)`, update shared, skip_wait=true
  - `TurnOff` → stop, set `state = Off`, update shared (skip TTS for web)
  - `TurnOn` → stop, set `state = Playing(last_channel)`, update shared, skip_wait=true
  - `TriggerUpdate` → spawn `updater::do_update()` in background thread
- `update_shared_state()` helper called after every state transition (both button and web)
- Track `last_channel: Option<usize>` for TurnOn
- `play_channel()` return type becomes `Option<RadioCommand>`, add `silent: bool` param

### Modify: `Cargo.toml`
- Add `tiny_http = "0.12"` dependency
- Add `os_pipe = "1"` dependency (OS-level pipe for SSE streaming)

### No changes: `src/button.rs`
Bridge thread in main.rs handles `ButtonEvent → RadioCommand` conversion.

## Main Loop Integration

```rust
// Unified channel
let (cmd_tx, cmd_rx) = mpsc::channel::<RadioCommand>();

// Button bridge (keeps button.rs unchanged)
let cmd_tx_btn = cmd_tx.clone();
std::thread::spawn(move || {
    for event in btn_rx {
        let cmd = match event {
            ButtonEvent::Down => RadioCommand::ButtonDown,
            ButtonEvent::Short => RadioCommand::ButtonShort,
            ButtonEvent::Long => RadioCommand::ButtonLong,
        };
        let _ = cmd_tx_btn.send(cmd);
    }
});

// Web server
web::start(shared_state.clone(), cmd_tx.clone());

// Main wait loop (replaces rx.recv())
loop {
    match cmd_rx.recv_timeout(Duration::from_millis(100)) {
        Ok(RadioCommand::SetVolume(v)) => {
            pipeline.set_volume(v);
            shared_state.volume.store(v, Ordering::SeqCst);
            continue; // Don't interrupt, keep waiting
        }
        Ok(cmd) => { /* handle command */ break; }
        Err(RecvTimeoutError::Timeout) => continue,
        Err(RecvTimeoutError::Disconnected) => return,
    }
}
```

## Edge Cases

- **Button held + web command:** Button hold loop consumes web command — button takes priority (acceptable, unlikely in practice)
- **SSE client disconnect:** Pipe write fails with broken pipe → pusher thread exits cleanly, connection count decremented
- **Volume affects beeps/TTS:** `sink.set_volume()` is global — matches real radio behavior
- **Pi 4B resources:** tiny-http lightweight, max 4 SSE threads, 300ms poll — negligible overhead
- **Update via web:** Spawned in background thread. Process exits and systemd restarts it.

## Implementation Order

1. `Cargo.toml` — add `tiny_http` and `os_pipe`
2. `src/commands.rs` — new: `RadioCommand` enum and `try_recv_command` helper
3. `src/web.rs` — new: HTTP server, SSE, embedded HTML
4. `src/audio.rs` — add `set_volume`, remove hardcoded constant, update `announce_interruptible` signature
5. `src/main.rs` — integrate: unified channel, shared state, web server startup, command handling
6. Test everything

## Verification

1. `cargo build` — compiles without errors
2. `cargo test` — all existing tests pass (button.rs unchanged)
3. `cargo run`, open `http://<pi-ip>:8080` on phone
4. Verify SSE: dev tools → Network → `/events` shows streaming `text/event-stream`
5. Verify volume slider: drag and release → POST sent → volume changes audibly
6. Verify channel tap: tap a channel → radio starts playing it (no TTS)
7. Verify on/off: tap Off → playback stops, tap On → resumes last channel
8. Verify state sync: two browser tabs → changes in one reflect in the other via SSE
9. Verify physical button still works alongside web UI
