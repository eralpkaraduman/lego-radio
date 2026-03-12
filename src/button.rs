use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use log::info;
use std::time::Duration;

/// Type of button press detected
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressType {
    /// Short press - cycle to next channel
    Short,
    /// Long press (held for 2+ seconds) - jump to Off state
    Long,
}

/// Duration to hold button for long press (milliseconds)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const LONG_PRESS_MS: u64 = 2000;

/// Trait for button input abstraction
pub trait ButtonInput: Send + Sync {
    fn wait_for_press(&self) -> PressType;
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
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const INPUT_DEBOUNCE_MS: u64 = 150;

/// Button configuration
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
    /// Flag for simulating long press (set by 'o' key)
    long_press: std::sync::atomic::AtomicBool,
}

impl KeyboardPinReader {
    pub fn new() -> Self {
        Self {
            pressed: std::sync::atomic::AtomicBool::new(false),
            long_press: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Check if last press was a long press simulation
    pub fn take_long_press(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.long_press.swap(false, Ordering::SeqCst)
    }
}

impl PinReader for KeyboardPinReader {
    fn read(&self) -> PinLevel {
        use std::sync::atomic::Ordering;

        // Poll for keyboard events (non-blocking)
        if event::poll(Duration::from_millis(1)).unwrap_or(false) {
            if let Ok(Event::Key(key_event)) = event::read() {
                match key_event {
                    // Enter or Space simulates short button press
                    KeyEvent {
                        code: KeyCode::Enter,
                        ..
                    }
                    | KeyEvent {
                        code: KeyCode::Char(' '),
                        ..
                    } => {
                        info!("Input: Key pressed (short)");
                        self.pressed.store(true, Ordering::SeqCst);
                    }
                    // 'o' key simulates long press (Off)
                    KeyEvent {
                        code: KeyCode::Char('o'),
                        modifiers: KeyModifiers::NONE,
                        ..
                    } => {
                        info!("Input: Key pressed (long/off)");
                        self.pressed.store(true, Ordering::SeqCst);
                        self.long_press.store(true, Ordering::SeqCst);
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
/// Supports:
/// - Enter/Space = short press (cycle channel)
/// - 'o' key = long press (jump to Off state)
pub struct KeyboardButton {
    reader: KeyboardPinReader,
}

impl KeyboardButton {
    pub fn new() -> Self {
        Self {
            reader: KeyboardPinReader::new(),
        }
    }
}

impl ButtonInput for KeyboardButton {
    fn wait_for_press(&self) -> PressType {
        // Wait for any key press
        loop {
            let level = self.reader.read();
            if level == PinLevel::Low {
                // Check if it was the 'o' key (long press simulation)
                if self.reader.take_long_press() {
                    info!("Keyboard: Long press (Off) detected");
                    return PressType::Long;
                }
                info!("Keyboard: Short press detected");
                return PressType::Short;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct GenericGpioButton<P: PinReader> {
    pin: P,
    config: ButtonConfig,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl<P: PinReader> GenericGpioButton<P> {
    pub fn new(pin: P, config: ButtonConfig) -> Self {
        Self { pin, config }
    }

    /// Wait for a debounced button press and detect short vs long press
    ///
    /// Behavior:
    /// 1. Wait for button press (Low)
    /// 2. Track how long button is held
    /// 3. When released, determine if short or long press based on hold duration
    /// 4. Apply debounce before returning
    pub fn wait_for_press_debounced(&self) -> PressType {
        use std::thread;
        use std::time::{Duration, Instant};

        let debounce = Duration::from_millis(self.config.debounce_ms);
        let long_press = Duration::from_millis(LONG_PRESS_MS);
        let poll = Duration::from_millis(self.config.poll_ms);

        // Wait for first press
        loop {
            let level = self.pin.read();
            if level == PinLevel::Low {
                info!("Button: Press detected, tracking hold duration...");
                break;
            }
            thread::sleep(poll);
        }

        // Track press duration
        let press_start = Instant::now();
        let mut last_press = Instant::now();

        loop {
            let level = self.pin.read();

            if level == PinLevel::Low {
                // Still pressing - update last press time
                last_press = Instant::now();
            }

            // Check if released (idle for debounce period)
            if last_press.elapsed() >= debounce {
                let hold_duration = last_press.duration_since(press_start);
                let press_type = if hold_duration >= long_press {
                    info!(
                        "Button: Long press registered ({:.1}s held)",
                        hold_duration.as_secs_f32()
                    );
                    PressType::Long
                } else {
                    info!(
                        "Button: Short press registered ({:.1}s held)",
                        hold_duration.as_secs_f32()
                    );
                    PressType::Short
                };
                return press_type;
            }

            thread::sleep(poll);
        }
    }
}

impl<P: PinReader> ButtonInput for GenericGpioButton<P> {
    fn wait_for_press(&self) -> PressType {
        self.wait_for_press_debounced()
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
    fn wait_for_press(&self) -> PressType {
        self.inner.wait_for_press()
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

    log::info!("Using keyboard input (Enter/Space=cycle, O=off)");
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
    fn test_short_press_detection() {
        // Short press should return PressType::Short
        let pin = MockPin::new(vec![
            PinLevel::Low,  // Press detected
            PinLevel::High, // Released quickly
            PinLevel::High, // Idle
            PinLevel::High, // Idle (debounce met)
        ]);
        let config = ButtonConfig {
            poll_ms: 1,
            debounce_ms: 2,
        };
        let button = GenericGpioButton::new(pin, config);

        let press_type = button.wait_for_press();
        assert_eq!(press_type, PressType::Short);
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

        let press_type = button.wait_for_press();
        // Only registers after spam stops, should be short press
        assert_eq!(press_type, PressType::Short);
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
    fn test_debounce_requires_sustained_idle() {
        // Press should NOT register until full debounce period passes
        // This test uses longer debounce to ensure idle period matters
        let pin = MockPin::new(vec![
            PinLevel::Low,  // Press
            PinLevel::High, // Release
            PinLevel::Low,  // Bounce! (within debounce window)
            PinLevel::High, // Release again
            PinLevel::High, // Idle 1
            PinLevel::High, // Idle 2
            PinLevel::High, // Idle 3 (debounce met after this)
        ]);
        let config = ButtonConfig {
            poll_ms: 1,
            debounce_ms: 3, // Requires 3 idle reads
        };

        let button = GenericGpioButton::new(pin, config);
        let press_type = button.wait_for_press();
        // Bouncy press still registers as single short press
        assert_eq!(press_type, PressType::Short);
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
        let press1 = button.wait_for_press();
        assert_eq!(press1, PressType::Short);

        // Second action
        let press2 = button.wait_for_press();
        assert_eq!(press2, PressType::Short);

        // Both completed - spam was debounced
    }

    #[test]
    fn test_press_type_equality() {
        assert_eq!(PressType::Short, PressType::Short);
        assert_eq!(PressType::Long, PressType::Long);
        assert_ne!(PressType::Short, PressType::Long);
    }

    #[test]
    fn test_long_press_constant() {
        // Long press threshold should be 2 seconds
        assert_eq!(LONG_PRESS_MS, 2000);
    }

    #[test]
    fn test_keyboard_pin_reader_long_press_flag() {
        use std::sync::atomic::Ordering;

        let reader = KeyboardPinReader::new();

        // Initially no long press
        assert!(!reader.take_long_press());

        // Set long press flag
        reader.long_press.store(true, Ordering::SeqCst);

        // First take should return true and clear
        assert!(reader.take_long_press());

        // Second take should return false (cleared)
        assert!(!reader.take_long_press());
    }

    #[test]
    fn test_keyboard_pin_reader_pressed_flag() {
        use std::sync::atomic::Ordering;

        let reader = KeyboardPinReader::new();

        // Initially not pressed (High)
        assert_eq!(reader.read(), PinLevel::High);

        // Set pressed flag
        reader.pressed.store(true, Ordering::SeqCst);

        // Should read Low and clear flag
        assert_eq!(reader.read(), PinLevel::Low);

        // Should be High again (flag cleared)
        assert_eq!(reader.read(), PinLevel::High);
    }

    // Note: GPIO long press detection relies on wall-clock time (2 seconds hold).
    // Testing this would require either:
    // 1. A 2+ second test (too slow for unit tests)
    // 2. Dependency injection for time/clock (added complexity)
    // The keyboard simulation tests above verify the flag mechanism works.
}
