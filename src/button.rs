use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
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
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
    fn wait_for_press(&self, tx: &std::sync::mpsc::Sender<ButtonEvent>) {
        // Wait for any key press
        loop {
            let level = self.reader.read();
            if level == PinLevel::Low {
                // Send Down event first
                let _ = tx.send(ButtonEvent::Down);

                // Check if it was the 'o' key (long press simulation)
                if self.reader.take_long_press() {
                    info!("Keyboard: Long press (Off) detected");
                    let _ = tx.send(ButtonEvent::Long);
                    return;
                }
                info!("Keyboard: Short press detected");
                let _ = tx.send(ButtonEvent::Short);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
            let level = self.pin.read();

            // Check if long press threshold reached while still held
            if press_start.elapsed() >= long_press_threshold && level == PinLevel::Low {
                info!(
                    "Button: Long press ({:.1}s) - triggering immediately",
                    press_start.elapsed().as_secs_f32()
                );
                let _ = tx.send(ButtonEvent::Long);
                return;
            }

            // Check if released before threshold
            if level == PinLevel::High {
                info!(
                    "Button: Short press ({:.1}s)",
                    press_start.elapsed().as_secs_f32()
                );
                let _ = tx.send(ButtonEvent::Short);
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

    // Note: test_long_press_triggers_while_held uses real wall-clock time (2 seconds).
    // For faster CI, this test is marked #[ignore] - run with `cargo test -- --ignored`
}
