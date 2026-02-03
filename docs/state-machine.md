# LEGO Radio State Machine

## Main State Flow

```mermaid
stateDiagram-v2
    [*] --> Boot

    state Boot {
        [*] --> InitLogger
        InitLogger --> InitTTS: Initialize TTS engine
        InitTTS --> InitAudio: Create audio player
        InitAudio --> InitButton: Spawn button listener thread
        InitButton --> [*]
    }

    Boot --> Welcome: channel_idx = 0

    state Welcome {
        [*] --> SayHello
        SayHello --> CheckingUpdates: "Hello!"
        CheckingUpdates --> UpdateCheck: "Checking for updates..."

        state UpdateCheck <<choice>>
        UpdateCheck --> Installing: Update available
        UpdateCheck --> UpToDate: No update

        Installing --> UpdateResult: "Installing..."

        state UpdateResult <<choice>>
        UpdateResult --> Restart: Success
        UpdateResult --> UpdateFailed: Failure

        Restart --> [*]: Exit & restart
        UpdateFailed --> WaitForButton: "Update failed"
        UpToDate --> WaitForButton: "Up to date"
        WaitForButton --> [*]: "Press button to start"
    }

    Welcome --> Channel1: Button press

    state Channel1 {
        [*] --> Stop1: Stop previous stream
        Stop1 --> Announce1: Fire & forget TTS
        Announce1 --> Stream1: Start stream (ducked 2s)
        Stream1 --> Playing1: Unduck after 2s
        Playing1 --> [*]
        note right of Playing1: YLE Classical
    }

    Channel1 --> Channel2: Button press

    state Channel2 {
        [*] --> Stop2: Stop previous stream
        Stop2 --> Announce2: Fire & forget TTS
        Announce2 --> Stream2: Start stream (ducked 2s)
        Stream2 --> Playing2: Unduck after 2s
        Playing2 --> [*]
        note right of Playing2: YLE Radio 1
    }

    Channel2 --> Channel3: Button press

    state Channel3 {
        [*] --> Stop3: Stop previous stream
        Stop3 --> Announce3: Fire & forget TTS
        Announce3 --> Stream3: Start stream (ducked 2s)
        Stream3 --> Playing3: Unduck after 2s
        Playing3 --> [*]
        note right of Playing3: Soma Groove Salad
    }

    Channel3 --> Channel4: Button press

    state Channel4 {
        [*] --> Stop4: Stop previous stream
        Stop4 --> Announce4: Fire & forget TTS
        Announce4 --> Stream4: Start stream (ducked 2s)
        Stream4 --> Playing4: Unduck after 2s
        Playing4 --> [*]
        note right of Playing4: Soma Drone Zone
    }

    Channel4 --> Off: Button press

    state Off {
        [*] --> StopStream: Stop stream
        StopStream --> SayOff: "Radio off"
        SayOff --> Idle
        Idle --> [*]
    }

    Off --> Welcome: Button press
```

## Button Input Handling

```mermaid
stateDiagram-v2
    [*] --> WaitingForPress

    state "Input Thread" as InputThread {
        WaitingForPress --> PressDetected: Pin goes LOW
        PressDetected --> Debouncing: Start 150ms timer

        state Debouncing {
            [*] --> Idle
            Idle --> ResetTimer: Another press detected
            ResetTimer --> Idle
            Idle --> Complete: 150ms elapsed with no press
        }

        Debouncing --> SendEvent: Debounce complete
        SendEvent --> WaitingForPress: Send to channel
    }

    state "Main Thread" as MainThread {
        Blocked --> ProcessPress: Receive from channel
        ProcessPress --> DrainExtras: Consume extra presses
        DrainExtras --> HandleChannel: Process channel change
        HandleChannel --> Blocked: Wait for next press
    }
```

## Audio Subsystem

```mermaid
stateDiagram-v2
    [*] --> Idle

    state "TTS Playback" as TTS {
        [*] --> Synthesize
        Synthesize --> PlaySamples: Piper returns samples
        Synthesize --> DirectPlay: macOS say (empty samples)
        PlaySamples --> [*]: Block until done
        DirectPlay --> [*]: Already played
    }

    state "Stream Playback" as Stream {
        [*] --> Connect: HTTP GET
        Connect --> Probe: Detect format (MP3/AAC)
        Probe --> Decode: Create decoder

        state Decode {
            [*] --> Ducked: Volume 10%
            Ducked --> Normal: After 2 seconds
            Normal --> ReadPacket
            ReadPacket --> DecodePacket
            DecodePacket --> PlayPacket
            PlayPacket --> ReadPacket: Loop
        }

        Decode --> [*]: Stop flag set
    }

    Idle --> TTS: speak() or speak_sync()
    Idle --> Stream: play_stream()
    Stream --> Idle: stop()
    TTS --> Idle: Complete
```

## Channel Index Logic

```
channel_idx | State
------------|------------------
-1          | Initial (before first iteration)
 0          | Welcome sequence
 1          | Channel 1 (YLE Classical)
 2          | Channel 2 (YLE Radio 1)
 3          | Channel 3 (Soma Groove Salad)
 4          | Channel 4 (Soma Drone Zone)
 5          | Radio Off
 6+         | Wraps to Welcome (0)
```

## Concurrency Model

```mermaid
flowchart TB
    subgraph "Main Thread"
        ML[Main Loop]
        ML --> |"rx.recv()"| WB[Wait for button]
        WB --> |"pending_press"| PC[Process channel]
        PC --> ML
    end

    subgraph "Input Thread"
        BL[Button Loop]
        BL --> |"wait_for_press()"| DB[Debounce 150ms]
        DB --> |"tx.send()"| BL
    end

    subgraph "Stream Thread"
        ST[Stream Audio]
        ST --> |"decode packets"| ST
        ST --> |"check stop_flag"| SE[End]
    end

    subgraph "TTS Thread (fire & forget)"
        TT[Synthesize]
        TT --> TP[Play samples]
        TP --> TE[End]
    end

    BL -.-> |"mpsc channel"| WB
    PC --> |"spawn"| ST
    PC --> |"spawn"| TT
```
