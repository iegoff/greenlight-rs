//! Physical gamepad support (via `gilrs`), including rumble.

use std::sync::mpsc;

use super::packet::VibrationReport;
use super::InputState;

/// Handle used to forward server rumble commands to the gamepad thread.
#[derive(Clone)]
pub struct RumbleSender(Option<mpsc::Sender<VibrationReport>>);

impl RumbleSender {
    pub fn disabled() -> Self {
        Self(None)
    }

    pub fn send(&self, v: VibrationReport) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(v);
        }
    }
}

#[cfg(not(feature = "gamepad"))]
pub fn spawn(_state: InputState, _deadzone: f32, _rumble: bool) -> RumbleSender {
    tracing::info!("built without gamepad support; keyboard input only");
    RumbleSender::disabled()
}

#[cfg(feature = "gamepad")]
pub fn spawn(state: InputState, deadzone: f32, rumble: bool) -> RumbleSender {
    let (tx, rx) = mpsc::channel::<VibrationReport>();
    let spawned = std::thread::Builder::new()
        .name("gamepad".into())
        .spawn(move || imp::run(state, deadzone, rumble, rx));
    match spawned {
        Ok(_) => RumbleSender(Some(tx)),
        Err(e) => {
            tracing::warn!("could not start gamepad thread: {e}");
            RumbleSender::disabled()
        }
    }
}

#[cfg(feature = "gamepad")]
mod imp {
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::time::Duration;

    use gilrs::ff::{BaseEffect, BaseEffectType, EffectBuilder, Replay, Ticks};
    use gilrs::{Axis, Button, EventType, GamepadId, Gilrs};

    use crate::input::packet::{buttons, GamepadFrame, VibrationReport};
    use crate::input::InputState;

    fn button_mask(b: Button) -> Option<u16> {
        Some(match b {
            Button::South => buttons::A,
            Button::East => buttons::B,
            Button::West => buttons::X,
            Button::North => buttons::Y,
            Button::DPadUp => buttons::DPAD_UP,
            Button::DPadDown => buttons::DPAD_DOWN,
            Button::DPadLeft => buttons::DPAD_LEFT,
            Button::DPadRight => buttons::DPAD_RIGHT,
            Button::LeftTrigger => buttons::LEFT_SHOULDER,
            Button::RightTrigger => buttons::RIGHT_SHOULDER,
            Button::LeftThumb => buttons::LEFT_THUMB,
            Button::RightThumb => buttons::RIGHT_THUMB,
            Button::Start => buttons::MENU,
            Button::Select => buttons::VIEW,
            Button::Mode => buttons::NEXUS,
            _ => return None,
        })
    }

    /// Keyboard/mouse receivers sometimes expose an extra HID interface that
    /// looks like a joystick (e.g. "... System Control"); ignore those.
    pub(super) fn looks_like_non_gamepad(name: &str) -> bool {
        let n = name.to_ascii_lowercase();
        ["system control", "consumer control", "keyboard", "mouse"].iter().any(|k| n.contains(k))
    }

    fn dz(v: f32, deadzone: f32) -> f32 {
        if v.abs() < deadzone { 0.0 } else { v }
    }

    pub fn run(state: InputState, deadzone: f32, rumble: bool, rx: mpsc::Receiver<VibrationReport>) {
        let mut gilrs = match Gilrs::new() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("gamepad support unavailable: {e}");
                return;
            }
        };
        for (_id, gp) in gilrs.gamepads() {
            if looks_like_non_gamepad(gp.name()) {
                tracing::info!("ignoring input device: {}", gp.name());
            } else {
                tracing::info!("gamepad connected: {} (ff: {})", gp.name(), gp.is_ff_supported());
            }
        }

        let mut frames: HashMap<GamepadId, GamepadFrame> = HashMap::new();
        let mut effects = Vec::new();

        loop {
            let mut dirty = false;
            while let Some(ev) = gilrs.next_event() {
                if looks_like_non_gamepad(gilrs.gamepad(ev.id).name()) {
                    continue;
                }
                let f = frames.entry(ev.id).or_insert_with(|| GamepadFrame::new(0));
                dirty = true;
                match ev.event {
                    EventType::ButtonPressed(b, _) | EventType::ButtonReleased(b, _) => {
                        let pressed = matches!(ev.event, EventType::ButtonPressed(..));
                        if let Some(m) = button_mask(b) {
                            f.set_button(m, pressed);
                        }
                    }
                    EventType::ButtonChanged(Button::LeftTrigger2, v, _) => f.left_trigger = v,
                    EventType::ButtonChanged(Button::RightTrigger2, v, _) => f.right_trigger = v,
                    EventType::AxisChanged(axis, v, _) => match axis {
                        Axis::LeftStickX => f.left_thumb_x = dz(v, deadzone),
                        Axis::LeftStickY => f.left_thumb_y = dz(v, deadzone),
                        Axis::RightStickX => f.right_thumb_x = dz(v, deadzone),
                        Axis::RightStickY => f.right_thumb_y = dz(v, deadzone),
                        Axis::LeftZ => f.left_trigger = v.max(0.0),
                        Axis::RightZ => f.right_trigger = v.max(0.0),
                        _ => {}
                    },
                    EventType::Connected => {
                        tracing::info!("gamepad connected: {}", gilrs.gamepad(ev.id).name());
                    }
                    EventType::Disconnected => {
                        frames.remove(&ev.id);
                    }
                    _ => {}
                }
            }
            if dirty {
                let merged = frames.values().fold(GamepadFrame::new(0), |acc, f| acc.merge(f));
                state.set_physical(merged);
            }

            while let Ok(v) = rx.try_recv() {
                if !rumble {
                    continue;
                }
                effects.clear();
                let ids: Vec<GamepadId> = gilrs
                    .gamepads()
                    .filter(|(_, g)| g.is_ff_supported() && !looks_like_non_gamepad(g.name()))
                    .map(|(id, _)| id)
                    .collect();
                if ids.is_empty() {
                    continue;
                }
                let strong = (v.left_motor.max(v.left_trigger_motor).clamp(0.0, 1.0) * u16::MAX as f32) as u16;
                let weak = (v.right_motor.max(v.right_trigger_motor).clamp(0.0, 1.0) * u16::MAX as f32) as u16;
                let play = Ticks::from_ms(v.duration_ms.max(1) as u32);
                let effect = EffectBuilder::new()
                    .add_effect(BaseEffect {
                        kind: BaseEffectType::Strong { magnitude: strong },
                        scheduling: Replay { play_for: play, with_delay: Ticks::from_ms(v.delay_ms as u32), ..Default::default() },
                        envelope: Default::default(),
                    })
                    .add_effect(BaseEffect {
                        kind: BaseEffectType::Weak { magnitude: weak },
                        scheduling: Replay { play_for: play, with_delay: Ticks::from_ms(v.delay_ms as u32), ..Default::default() },
                        envelope: Default::default(),
                    })
                    .gamepads(&ids)
                    .finish(&mut gilrs);
                match effect {
                    Ok(e) => {
                        let _ = e.play();
                        effects.push(e);
                    }
                    Err(e) => tracing::debug!("rumble failed: {e}"),
                }
            }

            std::thread::sleep(Duration::from_millis(4));
        }
    }
}

#[cfg(all(test, feature = "gamepad"))]
mod tests {
    #[test]
    fn filters_keyboard_receivers() {
        assert!(super::imp::looks_like_non_gamepad("NuPhy Air75 V3 Dongle System Control"));
        assert!(super::imp::looks_like_non_gamepad("Logitech USB Receiver Keyboard"));
        assert!(!super::imp::looks_like_non_gamepad("Steam Virtual Gamepad"));
        assert!(!super::imp::looks_like_non_gamepad("Xbox Wireless Controller"));
    }
}
