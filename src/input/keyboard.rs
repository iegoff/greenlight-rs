//! Keyboard → virtual gamepad mapping.
//!
//! Key names are the ones GStreamer navigation events use (X11 keysym names,
//! also used by the GL/Wayland/D3D sinks). Defaults follow Greenlight:
//!
//! ```text
//! D-pad: arrow keys     A: A / Enter      B: B / Backspace    X: X    Y: Y
//! Nexus: N              LB: [             RB: ]               LT: -   RT: =
//! View: V               Menu: M           LS: L               RS: R
//! ```

use super::packet::buttons;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mapped {
    Button(u16),
    LeftTrigger,
    RightTrigger,
}

pub fn map_key(key: &str) -> Option<Mapped> {
    use Mapped::*;
    let k = key.to_ascii_lowercase();
    Some(match k.as_str() {
        "a" | "return" | "kp_enter" | "enter" => Button(buttons::A),
        "b" | "backspace" => Button(buttons::B),
        "x" => Button(buttons::X),
        "y" => Button(buttons::Y),
        "up" | "kp_up" | "arrowup" => Button(buttons::DPAD_UP),
        "down" | "kp_down" | "arrowdown" => Button(buttons::DPAD_DOWN),
        "left" | "kp_left" | "arrowleft" => Button(buttons::DPAD_LEFT),
        "right" | "kp_right" | "arrowright" => Button(buttons::DPAD_RIGHT),
        "bracketleft" | "[" => Button(buttons::LEFT_SHOULDER),
        "bracketright" | "]" => Button(buttons::RIGHT_SHOULDER),
        "l" => Button(buttons::LEFT_THUMB),
        "r" => Button(buttons::RIGHT_THUMB),
        "m" => Button(buttons::MENU),
        "v" => Button(buttons::VIEW),
        "n" => Button(buttons::NEXUS),
        "minus" | "-" => LeftTrigger,
        "equal" | "=" => RightTrigger,
        _ => return None,
    })
}

/// Keys handled by the client itself rather than forwarded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClientAction {
    ToggleStats,
    RequestKeyframe,
    Quit,
}

pub fn client_action(key: &str) -> Option<ClientAction> {
    match key {
        "asciitilde" | "grave" | "~" | "`" | "dead_grave" => Some(ClientAction::ToggleStats),
        "F5" => Some(ClientAction::RequestKeyframe),
        "F12" => Some(ClientAction::Quit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        assert_eq!(map_key("Return"), Some(Mapped::Button(buttons::A)));
        assert_eq!(map_key("BackSpace"), Some(Mapped::Button(buttons::B)));
        assert_eq!(map_key("bracketleft"), Some(Mapped::Button(buttons::LEFT_SHOULDER)));
        assert_eq!(map_key("equal"), Some(Mapped::RightTrigger));
        assert_eq!(map_key("Up"), Some(Mapped::Button(buttons::DPAD_UP)));
        assert_eq!(map_key("q"), None);
        assert_eq!(client_action("asciitilde"), Some(ClientAction::ToggleStats));
    }
}
