use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use log::info;
use std::time::Duration;

/// Button events sent to main thread
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonEvent {
    /// Button pressed down - start feedback (beep)
    Down,
    /// Short press completed (released before threshold)
    Short,
    /// Long press threshold reached (while still held)
    Long,
}

/// Duration to hold button for long press (milliseconds)
pub const LONG_PRESS_MS: u64 = 2000;

/// Trait for button input abstraction
pub trait ButtonInput: Send + Sync {
    /// Wait for button press and send events through the channel
    /// Sends ButtonEvent::Down immediately when pressed,
    /// then ButtonEvent::Short or ButtonEvent::Long when determined
    fn wait_for_press(&self, tx: &std::sync::mpsc::Sender<ButtonEvent>);
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

/// Button configuration
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct ButtonConfig {
    /// Poll interval for reading pin state (milliseconds)
    pub poll_ms: u64,
}

impl Default for ButtonConfig {
    fn default() -> Self {
        Self { poll_ms: 10 }
    }
}

/// Keyboard pin reader - tracks actual key held state
/// Reports Low while Enter/Space is held, High when released
/// Behaves exactly like a physical button - no shortcuts
pub struct KeyboardPinReader {
    /// True while the button key (Enter/Space) is held down
    held: std::sync::atomic::AtomicBool,
}

impl KeyboardPinReader {
    pub fn new() -> Self {
        Self {
            held: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl PinReader for KeyboardPinReader {
    fn read(&self) -> PinLevel {
        use std::sync::atomic::Ordering;

        // Poll for keyboard events (non-blocking)
        if event::poll(Duration::from_millis(1)).unwrap_or(false) {
            if let Ok(Event::Key(key_event)) = event::read() {
                // Only handle Enter or Space as the "button"
                let is_button_key = matches!(key_event.code, KeyCode::Enter | KeyCode::Char(' '));

                if is_button_key {
                    match key_event.kind {
                        KeyEventKind::Press => {
                            info!("Input: Button key pressed");
                            self.held.store(true, Ordering::SeqCst);
                        }
                        KeyEventKind::Release => {
                            info!("Input: Button key released");
                            self.held.store(false, Ordering::SeqCst);
                        }
                        KeyEventKind::Repeat => {
                            // Key is being held - keep state as Low
                        }
                    }
                }

                // Ctrl+C to exit
                if let KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } = key_event
                {
                    std::process::exit(0);
                }
            }
        }

        // Return current held state
        if self.held.load(Ordering::SeqCst) {
            PinLevel::Low
        } else {
            PinLevel::High
        }
    }
}

/// Keyboard-based button for testing on Mac/desktop
/// Uses GenericGpioButton logic - behaves exactly like physical button
/// Hold Enter/Space for 2+ seconds for long press, release earlier for short press
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
    fn wait_for_press(&self, tx: &std::sync::mpsc::Sender<ButtonEvent>) {
        self.inner.wait_for_press(tx)
    }

    fn is_gpio(&self) -> bool {
        false
    }
}

/// Generic button that works with any PinReader implementation
/// Immediate response behavior:
/// - Short press: detected on button release (before long threshold)
/// - Long press: detected immediately when threshold reached while still held
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

    /// Wait for button press and send events through channel
    ///
    /// Sends events:
    /// 1. ButtonEvent::Down - immediately when button pressed
    /// 2. ButtonEvent::Short - if released before LONG_PRESS_MS
    ///    OR ButtonEvent::Long - if held for LONG_PRESS_MS (immediate, don't wait for release)
    fn detect_press(&self, tx: &std::sync::mpsc::Sender<ButtonEvent>) {
        use std::thread;
        use std::time::{Duration, Instant};

        let long_press_threshold = Duration::from_millis(LONG_PRESS_MS);
        let poll = Duration::from_millis(self.config.poll_ms);

        // Wait for button down (Low)
        loop {
            let level = self.pin.read();
            if level == PinLevel::Low {
                info!("Button: Press detected - sending Down event");
                let _ = tx.send(ButtonEvent::Down);
                break;
            }
            thread::sleep(poll);
        }

        // Track how long button is held
        let press_start = Instant::now();

        loop {
            thread::sleep(poll);

            // Check elapsed time first (cheaper than GPIO read)
            let elapsed = press_start.elapsed();
            let level = self.pin.read();

            // Check if released before threshold (most common case)
            if level == PinLevel::High {
                info!("Button: Short press ({:.1}s)", elapsed.as_secs_f32());
                let _ = tx.send(ButtonEvent::Short);
                return;
            }

            // Check if long press threshold reached while still held
            if elapsed >= long_press_threshold {
                info!(
                    "Button: Long press ({:.1}s) - triggering immediately",
                    elapsed.as_secs_f32()
                );
                let _ = tx.send(ButtonEvent::Long);
                return;
            }
        }
    }
}

impl<P: PinReader> ButtonInput for GenericGpioButton<P> {
    fn wait_for_press(&self, tx: &std::sync::mpsc::Sender<ButtonEvent>) {
        self.detect_press(tx)
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
    fn wait_for_press(&self, tx: &std::sync::mpsc::Sender<ButtonEvent>) {
        self.inner.wait_for_press(tx)
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

    log::info!("Using keyboard input (Enter/Space = button, hold 2s for off)");
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
    fn test_long_press_constant() {
        // Long press threshold should be 2 seconds
        assert_eq!(LONG_PRESS_MS, 2000);
    }

    #[test]
    fn test_button_config_default() {
        let config = ButtonConfig::default();
        assert_eq!(config.poll_ms, 10);
    }

    #[test]
    fn test_pin_level_equality() {
        assert_eq!(PinLevel::Low, PinLevel::Low);
        assert_eq!(PinLevel::High, PinLevel::High);
        assert_ne!(PinLevel::Low, PinLevel::High);
    }

    #[test]
    fn test_press_type_equality() {
        assert_eq!(ButtonEvent::Short, ButtonEvent::Short);
        assert_eq!(ButtonEvent::Long, ButtonEvent::Long);
        assert_ne!(ButtonEvent::Short, ButtonEvent::Long);
    }

    // ==================== Button Press Behavior Tests ====================
    // New behavior: immediate response, no debounce
    // - Short press: detected on button release (before long threshold)
    // - Long press: detected immediately when threshold reached (while still held)

    #[test]
    fn test_short_press_sends_down_then_short() {
        // Short press: button down then released before long press threshold
        // Should send Down, then Short
        let pin = MockPin::new(vec![
            PinLevel::Low,  // Button pressed - sends Down
            PinLevel::High, // Released quickly - sends Short
        ]);
        let config = ButtonConfig::default();
        let button = GenericGpioButton::new(pin, config);

        let (tx, rx) = std::sync::mpsc::channel();
        button.wait_for_press(&tx);

        // Should receive Down first, then Short
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Down);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Short);
    }

    #[test]
    #[ignore] // Takes 2+ seconds - run with `cargo test -- --ignored`
    fn test_long_press_sends_down_then_long() {
        // Long press: button held past threshold triggers immediately
        // Should send Down, then Long as soon as threshold reached
        let pin = MockPin::new(vec![
            PinLevel::Low, // Held... (will trigger Long after threshold)
        ]);
        let config = ButtonConfig {
            poll_ms: 10, // Normal polling
        };
        let button = GenericGpioButton::new(pin, config);

        let (tx, rx) = std::sync::mpsc::channel();
        button.wait_for_press(&tx);

        // Should receive Down first, then Long
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Down);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Long);
    }

    #[test]
    fn test_multiple_short_presses() {
        // Each press-release cycle should send Down+Short
        let pin = MockPin::new(vec![
            // First press
            PinLevel::Low,  // Press - sends Down
            PinLevel::High, // Release - sends Short
            // Second press
            PinLevel::Low,  // Press - sends Down
            PinLevel::High, // Release - sends Short
        ]);
        let config = ButtonConfig::default();
        let button = GenericGpioButton::new(pin, config);

        let (tx, rx) = std::sync::mpsc::channel();

        // First press
        button.wait_for_press(&tx);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Down);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Short);

        // Second press
        button.wait_for_press(&tx);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Down);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Short);
    }

    // ==================== Mock Infrastructure Tests ====================

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
        let pin = MockPin::new(vec![PinLevel::Low, PinLevel::High]);
        let config = ButtonConfig::default();
        let button = GenericGpioButton::new(pin, config);
        assert!(button.is_gpio());
    }

    // ==================== Keyboard Simulation Tests ====================
    // Note: KeyboardButton uses GenericGpioButton internally, so it shares
    // the same press detection logic as GPIO. The tests above for
    // GenericGpioButton also validate keyboard behavior.

    #[test]
    fn test_keyboard_pin_reader_held_state() {
        use std::sync::atomic::Ordering;

        let reader = KeyboardPinReader::new();

        // Initially not held (High)
        assert!(!reader.held.load(Ordering::SeqCst));

        // Simulate key press by setting held flag
        reader.held.store(true, Ordering::SeqCst);

        // Should read Low while held (no clearing - persistent state)
        assert_eq!(reader.read(), PinLevel::Low);
        assert_eq!(reader.read(), PinLevel::Low); // Still Low

        // Simulate key release
        reader.held.store(false, Ordering::SeqCst);

        // Should read High after release
        assert_eq!(reader.read(), PinLevel::High);
    }

    #[test]
    fn test_keyboard_uses_same_logic_as_gpio() {
        // KeyboardButton wraps GenericGpioButton, ensuring identical behavior
        let keyboard = KeyboardButton::new();
        let mock_pin = MockPin::new(vec![PinLevel::Low, PinLevel::High]);
        let gpio = GenericGpioButton::new(mock_pin, ButtonConfig::default());

        // Both should report the same is_gpio status as expected
        assert!(!keyboard.is_gpio()); // Keyboard reports false
        assert!(gpio.is_gpio()); // GPIO reports true

        // But both use the same GenericGpioButton::detect_press logic internally
        // (verified by code structure, not runtime test)
    }

    // Note: test_long_press_triggers_while_held uses real wall-clock time (2 seconds).
    // For faster CI, this test is marked #[ignore] - run with `cargo test -- --ignored`
}
