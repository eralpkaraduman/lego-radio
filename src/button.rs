use log::info;

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
#[allow(dead_code)]
pub trait ButtonInput: Send + Sync {
    /// Wait for button press and send events through the channel
    /// Sends ButtonEvent::Down immediately when pressed,
    /// then ButtonEvent::Short or ButtonEvent::Long when determined
    fn wait_for_press(&self, tx: &std::sync::mpsc::Sender<ButtonEvent>);
    fn is_gpio(&self) -> bool;
}

/// Pin level for GPIO (matches rppal::gpio::Level)
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

// =============================================================================
// GUI Button (macOS/desktop) - uses winit for proper press/release events
// =============================================================================

#[cfg(not(target_os = "linux"))]
mod gui_button {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Pin reader backed by a GUI window button.
    /// Mouse down on window → Low (pressed), mouse up → High (released).
    pub struct GuiPinReader {
        pressed: Arc<AtomicBool>,
    }

    impl PinReader for GuiPinReader {
        fn read(&self) -> PinLevel {
            if self.pressed.load(Ordering::SeqCst) {
                PinLevel::Low
            } else {
                PinLevel::High
            }
        }
    }

    /// Create a GuiPinReader and return it along with the shared pressed state.
    /// The caller must run `run_gui_window(pressed)` on the main thread.
    pub fn create_gui_pin_reader() -> (GuiPinReader, Arc<AtomicBool>) {
        let pressed = Arc::new(AtomicBool::new(false));
        let reader = GuiPinReader {
            pressed: pressed.clone(),
        };
        (reader, pressed)
    }

    // Embedded button sprites (104x99 RGBA, converted from PNG at build prep time)
    // Format: first 8 bytes = width(u32 LE) + height(u32 LE), rest = RGBA pixels
    const SPRITE_UP: &[u8] = include_bytes!("../assets/button_up.rgba");
    const SPRITE_DOWN: &[u8] = include_bytes!("../assets/button_down.rgba");

    fn load_sprite(data: &[u8]) -> (u32, u32, &[u8]) {
        let w = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let h = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        (w, h, &data[8..])
    }

    /// Run the GUI event loop. Must be called on the main thread (macOS requirement).
    /// Blocks forever.
    pub fn run_gui_window(pressed: Arc<AtomicBool>) {
        use softbuffer::Surface;
        use std::num::NonZeroU32;
        use winit::application::ApplicationHandler;
        use winit::event::{ElementState, MouseButton, WindowEvent};
        use winit::event_loop::{ActiveEventLoop, EventLoop};
        use winit::window::{Window, WindowAttributes, WindowId};

        let (sprite_w, sprite_h, _) = load_sprite(SPRITE_UP);

        const BG: u32 = 0xFF_2D2D2D;

        struct App {
            pressed: Arc<AtomicBool>,
            window: Option<std::rc::Rc<Window>>,
            surface: Option<Surface<std::rc::Rc<Window>, std::rc::Rc<Window>>>,
            is_down: bool,
            win_w: u32,
            win_h: u32,
            sprite_w: u32,
            sprite_h: u32,
            cursor_x: f64,
            cursor_y: f64,
        }

        fn draw(app: &mut App) {
            let (surface, window) = match (&mut app.surface, &app.window) {
                (Some(s), Some(w)) => (s, w),
                _ => return,
            };

            // Get physical pixel size (accounts for Retina/HiDPI)
            let scale = window.scale_factor();
            let phys = window.inner_size();
            let w = phys.width;
            let h = phys.height;

            if w == 0 || h == 0 {
                return;
            }

            surface
                .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
                .unwrap();

            let mut buf = surface.buffer_mut().unwrap();

            // Fill background
            for pixel in buf.iter_mut() {
                *pixel = BG;
            }

            // Pick sprite
            let (sw, sh, pixels) = if app.is_down {
                load_sprite(SPRITE_DOWN)
            } else {
                load_sprite(SPRITE_UP)
            };

            // Scale sprite to match physical pixels
            let scaled_w = (app.sprite_w as f64 * scale) as u32;
            let scaled_h = (app.sprite_h as f64 * scale) as u32;

            // Center in window
            let ox = (w.saturating_sub(scaled_w)) / 2;
            let oy = (h.saturating_sub(scaled_h)) / 2;

            // The sprite is black pixels with varying alpha (shadow overlay).
            // Render: fill a red ellipse, then composite scaled sprite on top.
            let base_r: u32 = 200;
            let base_g: u32 = 30;
            let base_b: u32 = 30;

            // First pass: fill ellipse with base red color
            let cx = ox + scaled_w / 2;
            let cy = oy + scaled_h / 2;
            let rx = (scaled_w / 2) as i32;
            let ry = (scaled_h / 2) as i32;
            for py in 0..h {
                for px in 0..w {
                    let ddx = px as i32 - cx as i32;
                    let ddy = py as i32 - cy as i32;
                    if ddx * ddx * ry * ry + ddy * ddy * rx * rx <= rx * rx * ry * ry {
                        buf[(py * w + px) as usize] = (base_r << 16) | (base_g << 8) | base_b;
                    }
                }
            }

            // Second pass: composite sprite scaled to physical pixels (nearest neighbor)
            for py in 0..scaled_h {
                for px in 0..scaled_w {
                    // Map back to sprite coordinates
                    let sx = (px as f64 / scale) as u32;
                    let sy = (py as f64 / scale) as u32;
                    let sx = sx.min(sw - 1);
                    let sy = sy.min(sh - 1);

                    let src_idx = ((sy * sw + sx) * 4) as usize;
                    if src_idx + 3 >= pixels.len() {
                        continue;
                    }
                    let sr = pixels[src_idx] as u32;
                    let sg = pixels[src_idx + 1] as u32;
                    let sb = pixels[src_idx + 2] as u32;
                    let a = pixels[src_idx + 3] as u32;

                    if a == 0 {
                        continue;
                    }

                    let dx = ox + px;
                    let dy = oy + py;
                    if dx >= w || dy >= h {
                        continue;
                    }

                    let dst_idx = (dy * w + dx) as usize;
                    let bg = buf[dst_idx];
                    let bg_r = (bg >> 16) & 0xFF;
                    let bg_g = (bg >> 8) & 0xFF;
                    let bg_b = bg & 0xFF;
                    let inv_a = 255 - a;
                    let out_r = (sr * a + bg_r * inv_a) / 255;
                    let out_g = (sg * a + bg_g * inv_a) / 255;
                    let out_b = (sb * a + bg_b * inv_a) / 255;
                    buf[dst_idx] = (out_r << 16) | (out_g << 8) | out_b;
                }
            }

            buf.present().unwrap();
        }

        impl ApplicationHandler for App {
            fn resumed(&mut self, event_loop: &ActiveEventLoop) {
                if self.window.is_none() {
                    let attrs = WindowAttributes::default()
                        .with_title("lego-radio")
                        .with_inner_size(winit::dpi::LogicalSize::new(
                            self.win_w as f64,
                            self.win_h as f64,
                        ))
                        .with_resizable(false)
                        .with_window_level(winit::window::WindowLevel::AlwaysOnTop);
                    match event_loop.create_window(attrs) {
                        Ok(w) => {
                            let w = std::rc::Rc::new(w);
                            let ctx = softbuffer::Context::new(w.clone()).unwrap();
                            let surface = Surface::new(&ctx, w.clone()).unwrap();
                            self.window = Some(w);
                            self.surface = Some(surface);
                        }
                        Err(e) => log::error!("Failed to create window: {}", e),
                    }
                }
            }

            fn window_event(
                &mut self,
                _event_loop: &ActiveEventLoop,
                _window_id: WindowId,
                event: WindowEvent,
            ) {
                match event {
                    WindowEvent::CursorMoved { position, .. } => {
                        self.cursor_x = position.x;
                        self.cursor_y = position.y;
                    }
                    WindowEvent::MouseInput {
                        state,
                        button: MouseButton::Left,
                        ..
                    } => {
                        match state {
                            ElementState::Pressed => {
                                info!("GUI: Button pressed");
                                self.pressed.store(true, Ordering::SeqCst);
                                self.is_down = true;
                            }
                            ElementState::Released if self.is_down => {
                                info!("GUI: Button released");
                                self.pressed.store(false, Ordering::SeqCst);
                                self.is_down = false;
                            }
                            _ => {}
                        }
                        draw(self);
                    }
                    WindowEvent::RedrawRequested => {
                        draw(self);
                    }
                    WindowEvent::CloseRequested => {
                        std::process::exit(0);
                    }
                    _ => {}
                }
            }
        }

        // Window slightly larger than sprite for padding
        let win_w = sprite_w + 40;
        let win_h = sprite_h + 40;

        let event_loop = EventLoop::new().expect("Failed to create event loop");
        let mut app = App {
            pressed,
            window: None,
            surface: None,
            is_down: false,
            win_w,
            win_h,
            sprite_w,
            sprite_h,
            cursor_x: 0.0,
            cursor_y: 0.0,
        };
        event_loop.run_app(&mut app).expect("Event loop failed");
    }

    pub struct GuiButton {
        inner: super::GenericGpioButton<GuiPinReader>,
    }

    impl GuiButton {
        pub fn new(pin_reader: GuiPinReader) -> Self {
            Self {
                inner: super::GenericGpioButton::new(pin_reader, ButtonConfig::default()),
            }
        }
    }

    impl ButtonInput for GuiButton {
        fn wait_for_press(&self, tx: &std::sync::mpsc::Sender<ButtonEvent>) {
            self.inner.wait_for_press(tx)
        }

        fn is_gpio(&self) -> bool {
            false
        }
    }
}

// =============================================================================
// Generic GPIO Button (shared logic for GPIO and GUI)
// =============================================================================

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

            let elapsed = press_start.elapsed();
            let level = self.pin.read();

            // Released before threshold → Short
            if level == PinLevel::High {
                info!("Button: Short press ({:.1}s)", elapsed.as_secs_f32());
                let _ = tx.send(ButtonEvent::Short);
                return;
            }

            // Held past threshold → Long (immediate)
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

// =============================================================================
// GPIO button for Raspberry Pi
// =============================================================================

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

// =============================================================================
// Factory
// =============================================================================

/// Create appropriate button input based on platform.
///
/// On Linux: uses GPIO pin 17.
/// On macOS: call `create_gui_button()` instead, which requires main-thread setup.
#[cfg(target_os = "linux")]
pub fn create_button() -> Box<dyn ButtonInput> {
    match GpioButton::new(17) {
        Ok(button) => {
            log::info!("Using GPIO button on pin 17");
            Box::new(button)
        }
        Err(e) => {
            panic!("GPIO not available: {}", e);
        }
    }
}

/// Create GUI button components for macOS/desktop.
/// Returns a ButtonInput and a closure that must be run on the main thread (blocks forever).
#[cfg(not(target_os = "linux"))]
pub fn create_gui_button() -> (Box<dyn ButtonInput>, impl FnOnce()) {
    let (pin_reader, pressed) = gui_button::create_gui_pin_reader();
    let button = Box::new(gui_button::GuiButton::new(pin_reader));
    let run_gui = move || {
        gui_button::run_gui_window(pressed);
    };
    (button, run_gui)
}

// =============================================================================
// Tests
// =============================================================================

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

    #[test]
    fn test_long_press_constant() {
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

    #[test]
    fn test_short_press_sends_down_then_short() {
        let pin = MockPin::new(vec![
            PinLevel::Low,  // Button pressed - sends Down
            PinLevel::High, // Released quickly - sends Short
        ]);
        let button = GenericGpioButton::new(pin, ButtonConfig::default());

        let (tx, rx) = std::sync::mpsc::channel();
        button.wait_for_press(&tx);

        assert_eq!(rx.recv().unwrap(), ButtonEvent::Down);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Short);
    }

    #[test]
    #[ignore] // Takes 2+ seconds
    fn test_long_press_sends_down_then_long() {
        let pin = MockPin::new(vec![
            PinLevel::Low, // Held forever → triggers Long after threshold
        ]);
        let button = GenericGpioButton::new(pin, ButtonConfig { poll_ms: 10 });

        let (tx, rx) = std::sync::mpsc::channel();
        button.wait_for_press(&tx);

        assert_eq!(rx.recv().unwrap(), ButtonEvent::Down);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Long);
    }

    #[test]
    fn test_multiple_short_presses() {
        let pin = MockPin::new(vec![
            PinLevel::Low,
            PinLevel::High,
            PinLevel::Low,
            PinLevel::High,
        ]);
        let button = GenericGpioButton::new(pin, ButtonConfig::default());

        let (tx, rx) = std::sync::mpsc::channel();

        button.wait_for_press(&tx);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Down);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Short);

        button.wait_for_press(&tx);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Down);
        assert_eq!(rx.recv().unwrap(), ButtonEvent::Short);
    }

    #[test]
    fn test_mock_pin_returns_sequence() {
        let pin = MockPin::new(vec![PinLevel::High, PinLevel::Low, PinLevel::High]);

        assert_eq!(pin.read(), PinLevel::High);
        assert_eq!(pin.read(), PinLevel::Low);
        assert_eq!(pin.read(), PinLevel::High);
        assert_eq!(pin.read(), PinLevel::High); // Repeats last
    }

    #[test]
    fn test_generic_gpio_button_is_gpio() {
        let pin = MockPin::new(vec![PinLevel::Low, PinLevel::High]);
        let button = GenericGpioButton::new(pin, ButtonConfig::default());
        assert!(button.is_gpio());
    }
}
