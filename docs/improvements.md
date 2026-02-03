# LEGO Radio - Code Review & Improvement Tasks

## Executive Summary

The codebase is functional but has areas of overengineering, logical gaps, and simplification opportunities. This document identifies issues and proposes improvements.

---

## 1. Overengineered Areas

### 1.1 Channel Index Logic is Confusing
**Problem:** Using `-1`, `0`, `1-N`, `N+1` for state management is error-prone.
```
-1 = initial (magic value)
 0 = welcome
 1-4 = channels
 5 = off
 6+ = wraps to 0
```

**Why it's overengineered:**
- Mixing "index" semantics with "state" semantics
- The `-1` initial value is a hack to make `+= 1` work on first iteration
- Wrap-around logic is implicit and scattered

**Task:** [ ] Replace integer index with explicit enum state machine
```rust
enum RadioState {
    Welcome,
    Playing(usize),  // channel index 0-3
    Off,
}
```

### 1.2 Two TTS Methods (`speak` vs `speak_sync`)
**Problem:** Having two methods with different signatures is confusing:
- `speak(&mut self, text: &str, tts: &Arc<PiperTts>)` - fire-and-forget
- `speak_sync(&mut self, text: &str, tts: &PiperTts)` - blocking

**Why it's overengineered:**
- Different type requirements (`Arc` vs plain ref)
- Caller must know which to use when
- Code duplication between the two methods

**Task:** [ ] Unify to single method with optional `blocking: bool` parameter, or always use `Arc`

### 1.3 Button Abstraction Layers
**Problem:** Multiple layers of abstraction for a simple button:
- `ButtonInput` trait
- `PinReader` trait
- `GenericGpioButton<P>`
- `KeyboardButton` (wraps GenericGpioButton)
- `GpioButton` (wraps GenericGpioButton)

**Why it's overengineered:**
- For a single button, this is excessive
- The `PinReader` trait adds indirection that's only useful for testing

**Task:** [ ] Consider simplifying - the traits are fine for testability, but could consolidate wrappers

---

## 2. Logical Loopholes & Edge Cases

### 2.1 Fire-and-Forget TTS Can Overlap
**Problem:** Multiple `speak()` calls can create overlapping audio:
```rust
player.speak("Channel 1", &tts);  // spawns thread
player.speak("Channel 2", &tts);  // spawns another thread immediately
// Both play simultaneously!
```

**Current mitigation:** Only used for channel announcements followed by stream start.

**Task:** [ ] Document this limitation or add TTS queue to prevent overlap

### 2.2 No Error Recovery for Stream Failures
**Problem:** If a stream fails to connect, user gets "Stream error" TTS but radio stays in that channel state. No automatic retry or fallback.

**Task:** [ ] Add retry logic or automatic advance to next channel on stream failure

### 2.3 Update Check Blocks Everything
**Problem:** During welcome sequence, update check is synchronous and can take several seconds. User cannot skip it.

**Task:** [ ] Consider making update check optional or add timeout

### 2.4 `pending_press` + `channel_idx` State Coupling
**Problem:** Two variables track related state, easy to get out of sync:
```rust
let mut channel_idx: i32 = -1;
let mut pending_press = true;
```

**Task:** [ ] Consolidate into single state struct

### 2.5 Stream Thread Not Tracked for TTS
**Problem:** `speak()` spawns threads that are never tracked or joined. If program exits during TTS, audio may cut off.

**Task:** [ ] Track TTS thread handles if graceful shutdown is needed

### 2.6 Main Thread Drains Extra Presses Silently
**Problem:** `while rx.try_recv().is_ok() {}` silently discards button presses that occurred during processing. User might not understand why rapid presses are ignored.

**Task:** [ ] Add debug logging for discarded presses

---

## 3. Completeness Issues

### 3.1 No Graceful Shutdown
**Problem:** No signal handler for SIGTERM/SIGINT. On `Ctrl+C`:
- Stream thread may be mid-packet
- TTS thread may be mid-speech
- No cleanup of resources

**Task:** [ ] Add signal handler to cleanly stop threads before exit

### 3.2 No Volume Control
**Problem:** Volume is hardcoded to 0.8. User cannot adjust.

**Task:** [ ] Consider adding volume control (button hold? config file?)

### 3.3 No Persistence
**Problem:** Radio always starts at Welcome. Doesn't remember last channel.

**Task:** [ ] Optional: Save/restore last channel to file

### 3.4 No Network Error Handling
**Problem:** If network is down during stream, error is logged but no user feedback about connectivity.

**Task:** [ ] Add TTS feedback for network errors: "Network unavailable"

### 3.5 Help Text Outdated
**Problem:** Help says `Channels cycle: 1 → 2 → 3 → ... → OFF → 1` but actual flow is `Welcome → 1 → 2 → 3 → 4 → OFF → Welcome`

**Task:** [ ] Update help text to match actual behavior

---

## 4. Documentation vs Code Mismatches

### 4.1 State Machine Diagram Shows Wrong Flow
**Problem in `state-machine.md`:**
- Shows `Channel1` has internal "Stop previous stream" but from Welcome there's no previous stream
- Diagram says "Restart → [*]: Exit & restart" but code does `process::exit(0)` (terminates, doesn't restart itself)

**Task:** [ ] Fix diagram accuracy

### 4.2 Channel Index Table Missing Context
**Problem:** Table shows indexes but doesn't explain the `+= 1` before check logic.

**Task:** [ ] Add explanation of increment-before-check pattern

---

## 5. Simplification Opportunities

### 5.1 Remove `pending_press` Variable
**Current:**
```rust
let mut pending_press = true;
loop {
    if !pending_press {
        if rx.recv().is_ok() {
            pending_press = true;
        }
    }
    if !pending_press { continue; }
    pending_press = false;
    // ... handle press
}
```

**Simpler:**
```rust
// Handle welcome on startup
handle_welcome();
// Then simple loop
loop {
    rx.recv().ok();  // Block for press
    while rx.try_recv().is_ok() {}  // Drain extras
    // ... handle press
}
```

**Task:** [ ] Refactor main loop to be simpler

### 5.2 Hardcoded 2-Second Duck Duration
**Problem:** `DUCK_DURATION_SECS = 2` may not match TTS length. Short announcements waste 2 seconds ducked; long announcements get cut off.

**Task:** [ ] Consider making duck duration configurable or dynamic

### 5.3 Redundant Stream Stop in Channel Switch
**Code:**
```rust
player.stop();           // Stops stream, waits for thread
player.speak(...);       // Fire-and-forget
player.play_stream();    // Calls stop() again internally!
```

**Task:** [ ] Remove redundant stop() call

---

## Priority Task List

### High Priority (Bugs/Correctness)
- [ ] Fix help text to match actual behavior
- [ ] Fix state machine diagram accuracy
- [ ] Remove redundant `player.stop()` call

### Medium Priority (Code Quality)
- [ ] Replace channel_idx integer with enum state
- [ ] Unify `speak` and `speak_sync` methods
- [ ] Simplify main loop by removing `pending_press`
- [ ] Add debug logging for discarded button presses

### Low Priority (Nice to Have)
- [ ] Add graceful shutdown signal handler
- [ ] Add network error TTS feedback
- [ ] Make duck duration configurable
- [ ] Consider channel persistence
- [ ] Add retry logic for failed streams

---

## Verdict

The code works but has accumulated complexity. The most impactful improvement would be replacing the integer-based state machine with an explicit enum, which would eliminate the `-1` hack, make state transitions explicit, and improve readability.
