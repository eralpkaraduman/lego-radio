use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use log::info;
use std::time::Duration;

/// Trait for button input abstraction
pub trait ButtonInput: Send + Sync {
    fn wait_for_press(&self);
    fn is_gpio(&self) -> bool;
}

/// Pin level for GPIO (matches rppal::gpio::Level)
/// Also used for keyboard simulation (High = not pressed, Low = pressed)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinLevel {
    Low,
    High,
}

/// Trait for abstracting GPIO pin reading (enables testing)
pub trait PinReader: Send + Sync {
    fn read(&self) -> PinLevel;
}

/// Single source of truth for input debounce duration
/// Time to wait after pressing stops before registering the action
pub const INPUT_DEBOUNCE_MS: u64 = 150;

/// Button configuration
#[derive(Debug, Clone, Copy)]
pub struct ButtonConfig {
    /// Poll interval for reading pin state
    pub poll_ms: u64,
    /// Debounce duration - input only registers after this much idle time (no activity)
    /// This prevents rapid repeated presses - user must stop pressing for this duration
    pub debounce_ms: u64,
}

impl Default for ButtonConfig {
    fn default() -> Self {
        Self {
            poll_ms: 10,
            debounce_ms: INPUT_DEBOUNCE_MS,
        }
    }
}

/// Keyboard pin reader - reports key presses as Low, otherwise High
/// Each key press sets a flag that is cleared after being read once as Low
pub struct KeyboardPinReader {
    /// Flag indicating a key was pressed (cleared after one Low read)
    pressed: std::sync::atomic::AtomicBool,
}

impl KeyboardPinReader {
    pub fn new() -> Self {
        Self {
            pressed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl PinReader for KeyboardPinReader {
    fn read(&self) -> PinLevel {
        use std::sync::atomic::Ordering;

        // Poll for keyboard events (non-blocking)
        if event::poll(Duration::from_millis(1)).unwrap_or(false) {
            if let Ok(Event::Key(key_event)) = event::read() {
                match key_event {
                    // Enter or Space simulates button press
                    KeyEvent {
                        code: KeyCode::Enter,
                        ..
                    }
                    | KeyEvent {
                        code: KeyCode::Char(' '),
                        ..
                    } => {
                        info!("Input: Key pressed");
                        self.pressed.store(true, Ordering::SeqCst);
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

        // Return Low if pressed, then clear the flag
        if self.pressed.swap(false, Ordering::SeqCst) {
            PinLevel::Low
        } else {
            PinLevel::High
        }
    }
}

/// Keyboard-based button for testing on Mac/desktop
/// Uses the same debounce state machine as GPIO for consistent behavior
pub struct KeyboardButton {
    inner: GenericGpioButton<KeyboardPinReader>,
}

impl KeyboardButton {
    pub fn new() -> Self {
        Self {
            inner: GenericGpioButton::new(KeyboardPinReader::new(), ButtonConfig::default()),
        }
    }
}

impl ButtonInput for KeyboardButton {
    fn wait_for_press(&self) {
        self.inner.wait_for_press();
    }

    fn is_gpio(&self) -> bool {
        false
    }
}

/// Generic button that works with any PinReader implementation
/// Uses trailing edge debounce:
/// - Press detected: start timer
/// - More presses: reset timer
/// - No presses for debounce_ms: register action
pub struct GenericGpioButton<P: PinReader> {
    pin: P,
    config: ButtonConfig,
}

impl<P: PinReader> GenericGpioButton<P> {
    pub fn new(pin: P, config: ButtonConfig) -> Self {
        Self { pin, config }
    }

    /// Wait for a debounced button press
    ///
    /// Behavior (trailing edge debounce):
    /// 1. Wait for first press
    /// 2. Each press resets the timer
    /// 3. Only register after NO presses for debounce_ms
    pub fn wait_for_press_debounced(&self) {
        use std::thread;
        use std::time::{Duration, Instant};

        let debounce = Duration::from_millis(self.config.debounce_ms);
        let poll = Duration::from_millis(self.config.poll_ms);

        // Wait for first press
        loop {
            let level = self.pin.read();
            if level == PinLevel::Low {
                info!(
                    "Button: Press detected, waiting for {}ms idle...",
                    self.config.debounce_ms
                );
                break;
            }
            thread::sleep(poll);
        }

        // Now wait for idle (no presses for debounce duration)
        let mut last_press = Instant::now();

        loop {
            let level = self.pin.read();

            if level == PinLevel::Low {
                // Still pressing or new press - reset timer
                last_press = Instant::now();
                info!("Button: Press detected, resetting idle timer...");
            }

            if last_press.elapsed() >= debounce {
                // Idle achieved - register the press
                info!(
                    "Button: Press registered (after {}ms idle)",
                    self.config.debounce_ms
                );
                return;
            }

            thread::sleep(poll);
        }
    }
}

impl<P: PinReader> ButtonInput for GenericGpioButton<P> {
    fn wait_for_press(&self) {
        self.wait_for_press_debounced();
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
            inner: GenericGpioButton::new(RppalPin { pin }, ButtonConfig::default()),
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

    log::info!("Using keyboard input (Enter/Space to cycle, with debounce)");
    Box::new(KeyboardButton::new())
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
            self.levels
                .get(idx)
                .copied()
                .unwrap_or(*self.levels.last().unwrap())
        }
    }

    // ==================== Configuration Tests ====================

    #[test]
    fn test_input_debounce_constant() {
        assert_eq!(INPUT_DEBOUNCE_MS, 150);
    }

    #[test]
    fn test_button_config_default() {
        let config = ButtonConfig::default();
        assert_eq!(config.poll_ms, 10);
        assert_eq!(config.debounce_ms, INPUT_DEBOUNCE_MS);
    }

    #[test]
    fn test_pin_level_equality() {
        assert_eq!(PinLevel::Low, PinLevel::Low);
        assert_eq!(PinLevel::High, PinLevel::High);
        assert_ne!(PinLevel::Low, PinLevel::High);
    }

    // ==================== Debounce Behavior Tests ====================

    #[test]
    fn test_press_waits_for_idle() {
        // Press should only register after idle period
        let pin = MockPin::new(vec![
            PinLevel::Low,  // Press detected
            PinLevel::High, // Released
            PinLevel::High, // Idle
            PinLevel::High, // Idle (debounce met)
        ]);
        let config = ButtonConfig {
            poll_ms: 1,
            debounce_ms: 2,
        };
        let button = GenericGpioButton::new(pin, config);

        button.wait_for_press();
        // If we get here, debounce worked
    }

    #[test]
    fn test_spam_resets_timer() {
        // Continuous pressing should reset the timer
        let pin = MockPin::new(vec![
            PinLevel::Low,  // Press 1
            PinLevel::Low,  // Press 2 (resets timer)
            PinLevel::Low,  // Press 3 (resets timer)
            PinLevel::High, // Release
            PinLevel::High, // Idle
            PinLevel::High, // Idle (debounce met)
        ]);
        let config = ButtonConfig {
            poll_ms: 1,
            debounce_ms: 2,
        };
        let button = GenericGpioButton::new(pin, config);

        button.wait_for_press();
        // Only registers after spam stops
    }

    // ==================== Integration Tests ====================

    #[test]
    fn test_mock_pin_returns_sequence() {
        let pin = MockPin::new(vec![PinLevel::High, PinLevel::Low, PinLevel::High]);

        assert_eq!(pin.read(), PinLevel::High);
        assert_eq!(pin.read(), PinLevel::Low);
        assert_eq!(pin.read(), PinLevel::High);
        assert_eq!(pin.read(), PinLevel::High); // Repeats last
    }

    #[test]
    fn test_keyboard_button_is_not_gpio() {
        let button = KeyboardButton::new();
        assert!(!button.is_gpio());
    }

    #[test]
    fn test_generic_gpio_button_is_gpio() {
        let pin = MockPin::new(vec![PinLevel::Low]);
        let config = ButtonConfig {
            poll_ms: 1,
            debounce_ms: 1,
        };
        let button = GenericGpioButton::new(pin, config);
        assert!(button.is_gpio());
    }

    #[test]
    fn test_wait_for_press_requires_idle_period() {
        // Should wait for debounce duration after last press
        let pin = MockPin::new(vec![
            PinLevel::Low,  // Press
            PinLevel::High, // Release
            PinLevel::High, // Idle
            PinLevel::High, // Idle (debounce met)
        ]);
        let config = ButtonConfig {
            poll_ms: 1,
            debounce_ms: 2,
        };

        let button = GenericGpioButton::new(pin, config);
        button.wait_for_press();
    }

    #[test]
    fn test_multiple_presses_only_one_action() {
        // Multiple rapid presses should result in one action after idle
        let pin = MockPin::new(vec![
            // First wait_for_press
            PinLevel::Low,  // Press
            PinLevel::Low,  // Spam
            PinLevel::Low,  // Spam
            PinLevel::High, // Release
            PinLevel::High, // Idle
            PinLevel::High, // Idle (debounce met) - returns here
            // Second wait_for_press
            PinLevel::High, // Idle
            PinLevel::Low,  // New press
            PinLevel::High, // Release
            PinLevel::High, // Idle
            PinLevel::High, // Idle (debounce met) - returns here
        ]);
        let config = ButtonConfig {
            poll_ms: 1,
            debounce_ms: 2,
        };
        let button = GenericGpioButton::new(pin, config);

        // First action
        button.wait_for_press();

        // Second action
        button.wait_for_press();

        // Both completed - spam was debounced
    }
}
