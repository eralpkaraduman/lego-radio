# LEGO Radio State Machine

## Main State Flow

```mermaid
stateDiagram-v2
    [*] --> Boot

    state Boot {
        [*] --> InitLogger
        InitLogger --> InitTTS
        InitTTS --> InitAudio
        InitAudio --> SpawnInputThread
        SpawnInputThread --> [*]
    }

    Boot --> Welcome: Auto (handle_welcome called)

    state Welcome {
        [*] --> SayHello: "Hello!"
        SayHello --> CheckUpdates: "Checking for updates..."

        state UpdateCheck <<choice>>
        CheckUpdates --> UpdateCheck
        UpdateCheck --> Installing: Update available
        UpdateCheck --> UpToDate: No update

        Installing --> InstallResult

        state InstallResult <<choice>>
        InstallResult --> Exit: Success
        InstallResult --> PromptStart: Failure

        Exit --> [*]: process::exit(0)\nsystemd restarts
        UpToDate --> PromptStart: "Up to date"
        PromptStart --> WaitForPress: "Press button to start"
        WaitForPress --> [*]
    }

    Welcome --> Playing: Button press

    state Playing {
        [*] --> StopPrevious: stop() if streaming
        StopPrevious --> AnnounceChannel: speak() fire-and-forget
        AnnounceChannel --> StartStream: play_stream()

        state StreamLoop {
            [*] --> Ducked: Volume 10%
            Ducked --> Normal: After 2 seconds
            Normal --> DecodeLoop
            DecodeLoop --> DecodeLoop: Read/decode/play packets
        }

        StartStream --> StreamLoop
        StreamLoop --> [*]: Playing until button press
    }

    note right of Playing
        Channels 1-4:
        1. YLE Classical
        2. YLE Radio 1
        3. Soma Groove Salad
        4. Soma Drone Zone
    end note

    Playing --> Playing: Button press\n(next channel)
    Playing --> Off: Button press\n(after last channel)

    state Off {
        [*] --> StopStream: stop()
        StopStream --> SayOff: "Radio off"
        SayOff --> Idle
        Idle --> [*]
    }

    Off --> Welcome: Button press
```

## State Machine (Enum)

The code uses a `RadioState` enum for explicit state management:

```rust
enum RadioState {
    Welcome,
    Playing(usize),  // channel index 0..N-1
    Off,
}

impl RadioState {
    fn next(self, num_channels: usize) -> RadioState {
        match self {
            Welcome => Playing(0),
            Playing(i) if i + 1 < num_channels => Playing(i + 1),
            Playing(_) => Off,
            Off => Welcome,
        }
    }
}
```

**State Transitions:**
```
Welcome → Playing(0) → Playing(1) → Playing(2) → Playing(3) → Off → Welcome
```

**On startup:** `handle_welcome()` is called immediately, then the loop waits for button presses.

## Button Input (Debounce)

```mermaid
stateDiagram-v2
    [*] --> WaitForPress

    state "Input Thread" as Input {
        WaitForPress --> PressDetected: Pin LOW

        state "Trailing Edge Debounce" as Debounce {
            [*] --> TimerRunning
            TimerRunning --> TimerRunning: Another press\n(reset to 0)
            TimerRunning --> Complete: 150ms idle
        }

        PressDetected --> Debounce
        Debounce --> SendToChannel: tx.send(())
        SendToChannel --> WaitForPress
    }

    state "Main Thread" as Main {
        Blocked --> GotPress: rx.recv()
        GotPress --> DrainExtras: Consume queued presses
        DrainExtras --> ProcessChannel
        ProcessChannel --> Blocked
    }

    Input --> Main: mpsc channel
```

**Debounce behavior:** Action only registers after user stops pressing for 150ms. Rapid presses reset the timer.

## Audio Subsystem

```mermaid
stateDiagram-v2
    [*] --> Idle

    state "TTS" as TTS {
        state "speak_sync() - Blocking" as Sync {
            [*] --> Synthesize1
            Synthesize1 --> Play1: Piper samples
            Synthesize1 --> Done1: macOS say (direct)
            Play1 --> Done1: sleep_until_end()
        }

        state "speak() - Fire & Forget" as Async {
            [*] --> SpawnThread
            SpawnThread --> Synthesize2
            Synthesize2 --> Play2: Piper samples
            Synthesize2 --> Done2: macOS say (direct)
            Play2 --> Done2
        }
    }

    state "Stream" as Stream {
        [*] --> HTTPConnect
        HTTPConnect --> ProbeFormat: MP3/AAC
        ProbeFormat --> CreateDecoder
        CreateDecoder --> PlayLoop

        state PlayLoop {
            [*] --> Ducked: 10% volume
            Ducked --> Normal: 2 sec elapsed
            Normal --> ReadPacket
            ReadPacket --> Decode
            Decode --> PlaySamples
            PlaySamples --> ReadPacket
        }

        PlayLoop --> [*]: stop_flag set
    }

    Idle --> TTS: speak() or speak_sync()
    Idle --> Stream: play_stream()
    Stream --> Idle: stop()
    TTS --> Idle: Complete
```

## Concurrency Model

```mermaid
flowchart TB
    subgraph "Main Thread"
        ML[Main Loop]
        ML --> WB[rx.recv - block]
        WB --> DR[Drain extras]
        DR --> PC[Process channel]
        PC --> ML
    end

    subgraph "Input Thread (spawned once)"
        BL[Button Loop]
        BL --> DP[Debounce 150ms]
        DP --> TX[tx.send]
        TX --> BL
    end

    subgraph "Stream Thread (per stream)"
        ST[Decode packets]
        ST --> CHK{stop_flag?}
        CHK -->|No| ST
        CHK -->|Yes| END[Exit]
    end

    subgraph "TTS Thread (fire & forget)"
        SYN[Synthesize]
        SYN --> PLY[Play samples]
        PLY --> DONE[Exit]
    end

    TX -.->|mpsc| WB
    PC -->|spawn| ST
    PC -->|spawn| SYN
```

## Implementation Notes

**Completed simplifications:**
- Replaced integer state with `RadioState` enum
- Removed `pending_press` variable - welcome handled on startup
- Removed redundant `stop()` call in channel switch
- Extracted `handle_welcome()` function for clarity

See `docs/improvements.md` for remaining improvement ideas.
