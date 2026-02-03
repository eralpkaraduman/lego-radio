use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use log::debug;
use std::time::Duration;

/// Trait for button input abstraction
pub trait ButtonInput: Send + Sync {
    fn wait_for_press(&self);
    fn is_gpio(&self) -> bool;
}

/// Pin level for GPIO (matches rppal::gpio::Level)
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinLevel {
    Low,
    High,
}

/// Trait for abstracting GPIO pin reading (enables testing)
#[cfg(any(target_os = "linux", test))]
pub trait PinReader: Send + Sync {
    fn read(&self) -> PinLevel;
}

/// Debounce configuration
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy)]
pub struct DebounceConfig {
    pub debounce_ms: u64,
    pub poll_ms: u64,
}

#[cfg(any(target_os = "linux", test))]
impl Default for DebounceConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 50,
            poll_ms: 10,
        }
    }
}

/// State machine for button press detection with debouncing
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    /// Waiting for initial button press
    WaitingForPress,
    /// Button appears pressed, debouncing
    Debouncing,
    /// Button confirmed pressed, waiting for release
    WaitingForRelease,
    /// Button released, debouncing release
    DebouncingRelease,
    /// Press cycle complete
    Complete,
}

/// Process one step of the button state machine
/// Returns the new state after processing the current pin level
#[cfg(any(target_os = "linux", test))]
pub fn process_button_state(
    state: ButtonState,
    pin_level: PinLevel,
    debounce_confirmed: bool,
) -> ButtonState {
    match state {
        ButtonState::WaitingForPress => {
            if pin_level == PinLevel::Low {
                ButtonState::Debouncing
            } else {
                ButtonState::WaitingForPress
            }
        }
        ButtonState::Debouncing => {
            if debounce_confirmed && pin_level == PinLevel::Low {
                ButtonState::WaitingForRelease
            } else if pin_level == PinLevel::High {
                // Button released during debounce - was a glitch
                ButtonState::WaitingForPress
            } else {
                ButtonState::Debouncing
            }
        }
        ButtonState::WaitingForRelease => {
            if pin_level == PinLevel::High {
                ButtonState::DebouncingRelease
            } else {
                ButtonState::WaitingForRelease
            }
        }
        ButtonState::DebouncingRelease => {
            if debounce_confirmed {
                ButtonState::Complete
            } else {
                ButtonState::DebouncingRelease
            }
        }
        ButtonState::Complete => ButtonState::Complete,
    }
}

/// Keyboard-based button for testing on Mac/desktop
pub struct KeyboardButton;

impl ButtonInput for KeyboardButton {
    fn wait_for_press(&self) {
        loop {
            // Poll for events with a timeout
            if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                if let Ok(Event::Key(key_event)) = event::read() {
                    match key_event {
                        // Enter key simulates button press
                        KeyEvent {
                            code: KeyCode::Enter,
                            ..
                        } => {
                            debug!("Keyboard: Enter pressed");
                            return;
                        }
                        // Space also works
                        KeyEvent {
                            code: KeyCode::Char(' '),
                            ..
                        } => {
                            debug!("Keyboard: Space pressed");
                            return;
                        }
                        // Ctrl+C to exit
                        KeyEvent {
                            code: KeyCode::Char('c'),
                            modifiers: KeyModifiers::CONTROL,
                            ..
                        } => {
                            std::process::exit(0);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn is_gpio(&self) -> bool {
        false
    }
}

/// Generic GPIO button that works with any PinReader implementation
#[cfg(any(target_os = "linux", test))]
pub struct GenericGpioButton<P: PinReader> {
    pin: P,
    config: DebounceConfig,
}

#[cfg(any(target_os = "linux", test))]
impl<P: PinReader> GenericGpioButton<P> {
    pub fn new(pin: P, config: DebounceConfig) -> Self {
        Self { pin, config }
    }

    /// Wait for a complete button press cycle (press -> release)
    pub fn wait_for_press_cycle(&self) {
        use std::thread;
        use std::time::{Duration, Instant};

        let debounce_duration = Duration::from_millis(self.config.debounce_ms);
        let poll_duration = Duration::from_millis(self.config.poll_ms);

        let mut state = ButtonState::WaitingForPress;
        let mut debounce_start: Option<Instant> = None;

        loop {
            let level = self.pin.read();
            let debounce_confirmed = debounce_start
                .map(|start| start.elapsed() >= debounce_duration)
                .unwrap_or(false);

            let new_state = process_button_state(state, level, debounce_confirmed);

            // Track debounce timing on state transitions
            if new_state != state {
                match new_state {
                    ButtonState::Debouncing | ButtonState::DebouncingRelease => {
                        debounce_start = Some(Instant::now());
                    }
                    ButtonState::WaitingForPress => {
                        debounce_start = None;
                    }
                    ButtonState::WaitingForRelease => {
                        debug!("GPIO: Button pressed");
                        debounce_start = None;
                    }
                    ButtonState::Complete => {
                        return;
                    }
                }
            }

            state = new_state;
            thread::sleep(poll_duration);
        }
    }
}

#[cfg(any(target_os = "linux", test))]
impl<P: PinReader> ButtonInput for GenericGpioButton<P> {
    fn wait_for_press(&self) {
        self.wait_for_press_cycle();
    }

    fn is_gpio(&self) -> bool {
        true
    }
}

/// GPIO button for Raspberry Pi using rppal
#[cfg(target_os = "linux")]
pub struct GpioButton {
    inner: GenericGpioButton<RppalPin>,
}

#[cfg(target_os = "linux")]
pub struct RppalPin {
    pin: rppal::gpio::InputPin,
}

#[cfg(target_os = "linux")]
impl PinReader for RppalPin {
    fn read(&self) -> PinLevel {
        use rppal::gpio::Level;
        match self.pin.read() {
            Level::Low => PinLevel::Low,
            Level::High => PinLevel::High,
        }
    }
}

#[cfg(target_os = "linux")]
impl GpioButton {
    pub fn new(pin_number: u8) -> Result<Self, rppal::gpio::Error> {
        use rppal::gpio::Gpio;

        let gpio = Gpio::new()?;
        let pin = gpio.get(pin_number)?.into_input_pullup();

        Ok(Self {
            inner: GenericGpioButton::new(
                RppalPin { pin },
                DebounceConfig::default(),
            ),
        })
    }
}

#[cfg(target_os = "linux")]
impl ButtonInput for GpioButton {
    fn wait_for_press(&self) {
        self.inner.wait_for_press();
    }

    fn is_gpio(&self) -> bool {
        true
    }
}

/// Create appropriate button input based on platform
pub fn create_button() -> Box<dyn ButtonInput> {
    #[cfg(target_os = "linux")]
    {
        // Try to create GPIO button (will fail on non-Pi Linux)
        match GpioButton::new(17) {
            Ok(button) => {
                log::info!("Using GPIO button on pin 17");
                return Box::new(button);
            }
            Err(e) => {
                log::warn!("GPIO not available ({}), using keyboard input", e);
            }
        }
    }

    log::info!("Using keyboard input (Enter/Space to cycle)");
    Box::new(KeyboardButton)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock pin reader that returns a sequence of levels
    struct MockPin {
        levels: Vec<PinLevel>,
        index: AtomicUsize,
    }

    impl MockPin {
        fn new(levels: Vec<PinLevel>) -> Self {
            Self {
                levels,
                index: AtomicUsize::new(0),
            }
        }
    }

    impl PinReader for MockPin {
        fn read(&self) -> PinLevel {
            let idx = self.index.fetch_add(1, Ordering::SeqCst);
            // Return the level at index, or last level if exhausted
            self.levels.get(idx).copied().unwrap_or(*self.levels.last().unwrap())
        }
    }

    // ==================== State Machine Tests ====================

    #[test]
    fn test_state_waiting_for_press_stays_high() {
        let state = process_button_state(ButtonState::WaitingForPress, PinLevel::High, false);
        assert_eq!(state, ButtonState::WaitingForPress);
    }

    #[test]
    fn test_state_waiting_for_press_detects_low() {
        let state = process_button_state(ButtonState::WaitingForPress, PinLevel::Low, false);
        assert_eq!(state, ButtonState::Debouncing);
    }

    #[test]
    fn test_state_debouncing_glitch_returns_to_waiting() {
        // If pin goes high during debounce, it was a glitch
        let state = process_button_state(ButtonState::Debouncing, PinLevel::High, false);
        assert_eq!(state, ButtonState::WaitingForPress);
    }

    #[test]
    fn test_state_debouncing_stays_until_confirmed() {
        // Still low but not confirmed yet
        let state = process_button_state(ButtonState::Debouncing, PinLevel::Low, false);
        assert_eq!(state, ButtonState::Debouncing);
    }

    #[test]
    fn test_state_debouncing_confirms_press() {
        // Low and debounce time passed
        let state = process_button_state(ButtonState::Debouncing, PinLevel::Low, true);
        assert_eq!(state, ButtonState::WaitingForRelease);
    }

    #[test]
    fn test_state_waiting_for_release_stays_low() {
        let state = process_button_state(ButtonState::WaitingForRelease, PinLevel::Low, false);
        assert_eq!(state, ButtonState::WaitingForRelease);
    }

    #[test]
    fn test_state_waiting_for_release_detects_high() {
        let state = process_button_state(ButtonState::WaitingForRelease, PinLevel::High, false);
        assert_eq!(state, ButtonState::DebouncingRelease);
    }

    #[test]
    fn test_state_debouncing_release_completes() {
        let state = process_button_state(ButtonState::DebouncingRelease, PinLevel::High, true);
        assert_eq!(state, ButtonState::Complete);
    }

    #[test]
    fn test_state_debouncing_release_waits() {
        let state = process_button_state(ButtonState::DebouncingRelease, PinLevel::High, false);
        assert_eq!(state, ButtonState::DebouncingRelease);
    }

    #[test]
    fn test_state_complete_stays_complete() {
        let state = process_button_state(ButtonState::Complete, PinLevel::High, false);
        assert_eq!(state, ButtonState::Complete);
    }

    // ==================== Full Press Cycle Tests ====================

    #[test]
    fn test_simple_button_press_sequence() {
        // Simulate: idle -> press -> hold -> release
        // With pull-up: High = idle, Low = pressed
        let levels = vec![
            PinLevel::High, // idle
            PinLevel::High, // idle
            PinLevel::Low,  // pressed (start debounce)
            PinLevel::Low,  // still pressed
            PinLevel::Low,  // still pressed
            PinLevel::Low,  // still pressed
            PinLevel::Low,  // still pressed (debounce done, confirmed)
            PinLevel::Low,  // held
            PinLevel::High, // released (start release debounce)
            PinLevel::High, // still released
            PinLevel::High, // still released
            PinLevel::High, // still released
            PinLevel::High, // release debounce done
        ];

        let mut state = ButtonState::WaitingForPress;
        let mut in_debounce_since: Option<usize> = None;
        let debounce_cycles = 4; // How many cycles to confirm debounce

        for (i, &level) in levels.iter().enumerate() {
            let debounce_confirmed = in_debounce_since
                .map(|start| i - start >= debounce_cycles)
                .unwrap_or(false);

            let new_state = process_button_state(state, level, debounce_confirmed);

            // Track debounce timing
            if new_state != state {
                match new_state {
                    ButtonState::Debouncing | ButtonState::DebouncingRelease => {
                        in_debounce_since = Some(i);
                    }
                    _ => {
                        in_debounce_since = None;
                    }
                }
            }

            state = new_state;

            if state == ButtonState::Complete {
                break;
            }
        }

        assert_eq!(state, ButtonState::Complete);
    }

    #[test]
    fn test_glitch_rejection() {
        // Simulate a glitch: brief low then back to high
        let mut state = ButtonState::WaitingForPress;

        // Pin goes low briefly
        state = process_button_state(state, PinLevel::Low, false);
        assert_eq!(state, ButtonState::Debouncing);

        // Pin goes back high before debounce completes (glitch)
        state = process_button_state(state, PinLevel::High, false);
        assert_eq!(state, ButtonState::WaitingForPress);

        // Should be back to waiting
        state = process_button_state(state, PinLevel::High, false);
        assert_eq!(state, ButtonState::WaitingForPress);
    }

    #[test]
    fn test_bouncy_button_press() {
        // Simulate mechanical bounce: low-high-low-high-low (settles low)
        let mut state = ButtonState::WaitingForPress;

        // First bounce - goes low
        state = process_button_state(state, PinLevel::Low, false);
        assert_eq!(state, ButtonState::Debouncing);

        // Bounce back high - rejected as glitch
        state = process_button_state(state, PinLevel::High, false);
        assert_eq!(state, ButtonState::WaitingForPress);

        // Second bounce - goes low again
        state = process_button_state(state, PinLevel::Low, false);
        assert_eq!(state, ButtonState::Debouncing);

        // Bounce back high again
        state = process_button_state(state, PinLevel::High, false);
        assert_eq!(state, ButtonState::WaitingForPress);

        // Finally settles low
        state = process_button_state(state, PinLevel::Low, false);
        assert_eq!(state, ButtonState::Debouncing);

        // Stays low through debounce
        state = process_button_state(state, PinLevel::Low, false);
        assert_eq!(state, ButtonState::Debouncing);

        // Debounce confirmed
        state = process_button_state(state, PinLevel::Low, true);
        assert_eq!(state, ButtonState::WaitingForRelease);
    }

    // ==================== Integration Tests ====================

    #[test]
    fn test_debounce_config_default() {
        let config = DebounceConfig::default();
        assert_eq!(config.debounce_ms, 50);
        assert_eq!(config.poll_ms, 10);
    }

    #[test]
    fn test_pin_level_equality() {
        assert_eq!(PinLevel::Low, PinLevel::Low);
        assert_eq!(PinLevel::High, PinLevel::High);
        assert_ne!(PinLevel::Low, PinLevel::High);
    }

    #[test]
    fn test_keyboard_button_is_not_gpio() {
        let button = KeyboardButton;
        assert!(!button.is_gpio());
    }

    #[test]
    fn test_mock_pin_returns_sequence() {
        let pin = MockPin::new(vec![PinLevel::High, PinLevel::Low, PinLevel::High]);

        assert_eq!(pin.read(), PinLevel::High);
        assert_eq!(pin.read(), PinLevel::Low);
        assert_eq!(pin.read(), PinLevel::High);
        // After exhausting, returns last
        assert_eq!(pin.read(), PinLevel::High);
    }

    // ==================== Property-Based Observations ====================

    #[test]
    fn test_state_machine_never_skips_states() {
        // From WaitingForPress, can only go to Debouncing or stay
        let from_waiting = [
            process_button_state(ButtonState::WaitingForPress, PinLevel::High, false),
            process_button_state(ButtonState::WaitingForPress, PinLevel::Low, false),
            process_button_state(ButtonState::WaitingForPress, PinLevel::High, true),
            process_button_state(ButtonState::WaitingForPress, PinLevel::Low, true),
        ];
        for state in from_waiting {
            assert!(
                state == ButtonState::WaitingForPress || state == ButtonState::Debouncing,
                "Invalid transition from WaitingForPress"
            );
        }

        // From Debouncing, can go to WaitingForPress, stay, or WaitingForRelease
        let from_debouncing = [
            process_button_state(ButtonState::Debouncing, PinLevel::High, false),
            process_button_state(ButtonState::Debouncing, PinLevel::Low, false),
            process_button_state(ButtonState::Debouncing, PinLevel::Low, true),
        ];
        for state in from_debouncing {
            assert!(
                state == ButtonState::WaitingForPress
                    || state == ButtonState::Debouncing
                    || state == ButtonState::WaitingForRelease,
                "Invalid transition from Debouncing"
            );
        }

        // From WaitingForRelease, can only go to DebouncingRelease or stay
        let from_waiting_release = [
            process_button_state(ButtonState::WaitingForRelease, PinLevel::High, false),
            process_button_state(ButtonState::WaitingForRelease, PinLevel::Low, false),
        ];
        for state in from_waiting_release {
            assert!(
                state == ButtonState::WaitingForRelease || state == ButtonState::DebouncingRelease,
                "Invalid transition from WaitingForRelease"
            );
        }

        // From DebouncingRelease, can only go to Complete or stay
        let from_debouncing_release = [
            process_button_state(ButtonState::DebouncingRelease, PinLevel::High, false),
            process_button_state(ButtonState::DebouncingRelease, PinLevel::High, true),
        ];
        for state in from_debouncing_release {
            assert!(
                state == ButtonState::DebouncingRelease || state == ButtonState::Complete,
                "Invalid transition from DebouncingRelease"
            );
        }
    }

    #[test]
    fn test_complete_requires_full_cycle() {
        // Cannot reach Complete without going through all states
        // Direct transitions to Complete should be impossible
        assert_ne!(
            process_button_state(ButtonState::WaitingForPress, PinLevel::Low, true),
            ButtonState::Complete
        );
        assert_ne!(
            process_button_state(ButtonState::Debouncing, PinLevel::Low, true),
            ButtonState::Complete
        );
        assert_ne!(
            process_button_state(ButtonState::WaitingForRelease, PinLevel::High, true),
            ButtonState::Complete
        );
    }

    // ==================== GenericGpioButton Integration Tests ====================

    #[test]
    fn test_generic_gpio_button_is_gpio() {
        // Create a mock pin that simulates: idle -> press -> release
        let levels = vec![
            PinLevel::High, // Idle
            PinLevel::Low,  // Press detected
            PinLevel::Low,  // Debouncing
            PinLevel::Low,  // Debouncing
            PinLevel::Low,  // Debouncing
            PinLevel::Low,  // Debouncing
            PinLevel::Low,  // Confirmed press
            PinLevel::High, // Release detected
            PinLevel::High, // Debouncing release
            PinLevel::High, // Debouncing release
            PinLevel::High, // Debouncing release
            PinLevel::High, // Debouncing release
            PinLevel::High, // Complete
        ];

        let pin = MockPin::new(levels);
        let config = DebounceConfig {
            debounce_ms: 1, // Use very short debounce for tests
            poll_ms: 1,
        };

        let button = GenericGpioButton::new(pin, config);
        assert!(button.is_gpio());
    }

    #[test]
    fn test_generic_gpio_button_wait_for_press() {
        // Create a mock pin that simulates a clean button press
        let levels = vec![
            PinLevel::Low,  // Press detected immediately
            PinLevel::Low,  // Debouncing
            PinLevel::Low,  // Debouncing
            PinLevel::Low,  // Debouncing
            PinLevel::Low,  // Debouncing
            PinLevel::Low,  // Confirmed
            PinLevel::High, // Release detected
            PinLevel::High, // Debouncing release
            PinLevel::High, // Debouncing release
            PinLevel::High, // Debouncing release
            PinLevel::High, // Debouncing release
            PinLevel::High, // Complete
        ];

        let pin = MockPin::new(levels);
        let config = DebounceConfig {
            debounce_ms: 1,
            poll_ms: 1,
        };

        let button = GenericGpioButton::new(pin, config);

        // This should complete without blocking forever
        button.wait_for_press();
        // If we get here, the test passed
    }
}
