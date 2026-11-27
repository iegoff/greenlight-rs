pub mod gamepad;
pub mod keyboard;
pub mod packet;

use std::sync::{Arc, Mutex};

use packet::GamepadFrame;

/// Shared virtual-gamepad state, written by input sources (keyboard, physical
/// pads) and read by the input-channel loop.
#[derive(Clone, Default)]
pub struct InputState {
    inner: Arc<Mutex<Sources>>,
}

#[derive(Default)]
struct Sources {
    keyboard: GamepadFrame,
    physical: GamepadFrame,
    /// Buttons injected by the UI (e.g. the overlay's Xbox button).
    overlay: GamepadFrame,
}

impl InputState {
    pub fn update_keyboard(&self, f: impl FnOnce(&mut GamepadFrame)) {
        f(&mut self.inner.lock().unwrap().keyboard);
    }

    pub fn set_overlay(&self, frame: GamepadFrame) {
        self.inner.lock().unwrap().overlay = frame;
    }

    pub fn set_physical(&self, frame: GamepadFrame) {
        self.inner.lock().unwrap().physical = frame;
    }

    /// Merged state of all sources for virtual gamepad 0.
    pub fn snapshot(&self) -> GamepadFrame {
        let s = self.inner.lock().unwrap();
        let mut f = s.physical.merge(&s.keyboard).merge(&s.overlay);
        f.gamepad_index = 0;
        f
    }
}
