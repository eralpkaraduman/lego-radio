use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use log::debug;
use std::time::Duration;

/// Trait for button input abstraction
pub trait ButtonInput: Send + Sync {
    fn wait_for_press(&self);
    fn is_gpio(&self) -> bool;
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

/// GPIO button for Raspberry Pi
#[cfg(target_os = "linux")]
pub struct GpioButton {
    pin: rppal::gpio::InputPin,
}

#[cfg(target_os = "linux")]
impl GpioButton {
    pub fn new(pin_number: u8) -> Result<Self, rppal::gpio::Error> {
        use rppal::gpio::Gpio;

        let gpio = Gpio::new()?;
        let pin = gpio.get(pin_number)?.into_input_pullup();

        Ok(Self { pin })
    }
}

#[cfg(target_os = "linux")]
impl ButtonInput for GpioButton {
    fn wait_for_press(&self) {
        use rppal::gpio::Level;
        use std::thread;
        use std::time::Duration;

        // Debounce: wait for stable low signal
        let debounce_time = Duration::from_millis(50);

        loop {
            // Wait for button press (pin goes LOW with pull-up)
            if self.pin.read() == Level::Low {
                // Debounce
                thread::sleep(debounce_time);

                // Confirm still pressed
                if self.pin.read() == Level::Low {
                    debug!("GPIO: Button pressed");

                    // Wait for release
                    while self.pin.read() == Level::Low {
                        thread::sleep(Duration::from_millis(10));
                    }

                    // Debounce release
                    thread::sleep(debounce_time);

                    return;
                }
            }

            thread::sleep(Duration::from_millis(10));
        }
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
