//! Binary input report encoding/decoding for the `input` data channel.
//!
//! Wire format (all little endian unless noted):
//!
//! ```text
//! u16  report type bitmask (see [`ReportType`])
//! u32  sequence number
//! f64  client timestamp (ms)
//! ...  sections, in order: metadata, gamepad, pointer, mouse, keyboard
//! u8   max touch points (client-metadata reports only)
//! ```

use std::time::Instant;

/// Report type bit flags.
#[allow(dead_code)]
pub mod report_type {
    pub const NONE: u16 = 0;
    pub const METADATA: u16 = 1;
    pub const GAMEPAD: u16 = 2;
    pub const POINTER: u16 = 4;
    pub const CLIENT_METADATA: u16 = 8;
    pub const SERVER_METADATA: u16 = 16;
    pub const MOUSE: u16 = 32;
    pub const KEYBOARD: u16 = 64;
    pub const VIBRATION: u16 = 128;
    pub const SENSOR: u16 = 256;
    pub const UNRELIABLE_INPUT_REPORT: u16 = 512;
    pub const UNRELIABLE_INPUT_ACK: u16 = 1024;
}

pub const HEADER_SIZE: usize = 14;

/// Gamepad button bitmask values.
pub mod buttons {
    pub const NEXUS: u16 = 2;
    pub const MENU: u16 = 4;
    pub const VIEW: u16 = 8;
    pub const A: u16 = 16;
    pub const B: u16 = 32;
    pub const X: u16 = 64;
    pub const Y: u16 = 128;
    pub const DPAD_UP: u16 = 256;
    pub const DPAD_DOWN: u16 = 512;
    pub const DPAD_LEFT: u16 = 1024;
    pub const DPAD_RIGHT: u16 = 2048;
    pub const LEFT_SHOULDER: u16 = 4096;
    pub const RIGHT_SHOULDER: u16 = 8192;
    pub const LEFT_THUMB: u16 = 16384;
    pub const RIGHT_THUMB: u16 = 32768;
}

/// State of a single virtual gamepad. Axes are in `-1.0..=1.0` and follow
/// the browser Gamepad API, as in Greenlight: X right = +1, **Y up = -1**.
/// The encoder negates Y, so "up" goes on the wire as +32767. Triggers are
/// in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GamepadFrame {
    pub gamepad_index: u8,
    /// Bitmask of [`buttons`].
    pub buttons: u16,
    pub left_thumb_x: f32,
    pub left_thumb_y: f32,
    pub right_thumb_x: f32,
    pub right_thumb_y: f32,
    pub left_trigger: f32,
    pub right_trigger: f32,
}

impl GamepadFrame {
    pub fn new(index: u8) -> Self {
        Self { gamepad_index: index, ..Default::default() }
    }

    pub fn set_button(&mut self, mask: u16, pressed: bool) {
        if pressed {
            self.buttons |= mask;
        } else {
            self.buttons &= !mask;
        }
    }

    /// Merges another source into this frame: buttons are OR-ed, for axes the
    /// value with the larger magnitude wins and triggers take the maximum.
    pub fn merge(&self, other: &GamepadFrame) -> GamepadFrame {
        fn axis(a: f32, b: f32) -> f32 {
            if a.abs() >= b.abs() { a } else { b }
        }
        GamepadFrame {
            gamepad_index: self.gamepad_index,
            buttons: self.buttons | other.buttons,
            left_thumb_x: axis(self.left_thumb_x, other.left_thumb_x),
            left_thumb_y: axis(self.left_thumb_y, other.left_thumb_y),
            right_thumb_x: axis(self.right_thumb_x, other.right_thumb_x),
            right_thumb_y: axis(self.right_thumb_y, other.right_thumb_y),
            left_trigger: self.left_trigger.max(other.left_trigger),
            right_trigger: self.right_trigger.max(other.right_trigger),
        }
    }
}

/// Video frame timing metadata reported back to the server for latency tracking.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MetadataFrame {
    pub server_data_key: u32,
    pub first_frame_packet_arrival_time_ms: u32,
    pub frame_submitted_time_ms: u32,
    pub frame_decoded_time_ms: u32,
    pub frame_rendered_time_ms: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MouseFrame {
    pub x: i32,
    pub y: i32,
    pub wheel_x: i32,
    pub wheel_y: i32,
    pub buttons: u8,
    /// 0 = relative, 1 = absolute
    pub relative: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct KeyboardFrame {
    pub pressed: bool,
    /// Windows virtual key code.
    pub key_code: u8,
}

/// Monotonic millisecond clock shared by all packets (mirrors `performance.now()`).
pub fn now_ms() -> f64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

#[derive(Debug, Clone, Default)]
pub struct InputPacket {
    pub sequence: u32,
    pub metadata: Vec<MetadataFrame>,
    pub gamepads: Vec<GamepadFrame>,
    pub mouse: Vec<MouseFrame>,
    pub keyboard: Vec<KeyboardFrame>,
    /// When set, this is a client-metadata report carrying the max touchpoints.
    pub client_metadata: Option<u8>,
}

impl InputPacket {
    pub fn new(sequence: u32) -> Self {
        Self { sequence, ..Default::default() }
    }

    /// Client-metadata report, sent once when the input channel starts.
    pub fn client_metadata(sequence: u32, max_touchpoints: u8) -> Self {
        Self { sequence, client_metadata: Some(max_touchpoints), ..Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.metadata.is_empty()
            && self.gamepads.is_empty()
            && self.mouse.is_empty()
            && self.keyboard.is_empty()
            && self.client_metadata.is_none()
    }

    pub fn report_type(&self) -> u16 {
        if self.client_metadata.is_some() {
            return report_type::CLIENT_METADATA;
        }
        let mut t = report_type::NONE;
        if !self.metadata.is_empty() {
            t |= report_type::METADATA;
        }
        if !self.gamepads.is_empty() {
            t |= report_type::GAMEPAD;
        }
        if !self.mouse.is_empty() {
            t |= report_type::MOUSE;
        }
        if !self.keyboard.is_empty() {
            t |= report_type::KEYBOARD;
        }
        t
    }

    pub fn encoded_len(&self) -> usize {
        if self.client_metadata.is_some() {
            return HEADER_SIZE + 1;
        }
        let mut size = HEADER_SIZE;
        if !self.metadata.is_empty() {
            size += 1 + 28 * self.metadata.len();
        }
        if !self.gamepads.is_empty() {
            size += 1 + 23 * self.gamepads.len();
        }
        if !self.mouse.is_empty() {
            size += 1 + 18 * self.mouse.len();
        }
        if !self.keyboard.is_empty() {
            size += 1 + 3 * self.keyboard.len();
        }
        size
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_bytes_at(now_ms())
    }

    /// Encodes the packet using `timestamp_ms` as the client clock.
    pub fn to_bytes_at(&self, timestamp_ms: f64) -> Vec<u8> {
        let mut b = Vec::with_capacity(self.encoded_len());
        b.extend_from_slice(&self.report_type().to_le_bytes());
        b.extend_from_slice(&self.sequence.to_le_bytes());
        b.extend_from_slice(&timestamp_ms.to_le_bytes());

        if let Some(touchpoints) = self.client_metadata {
            b.push(touchpoints);
            debug_assert_eq!(b.len(), self.encoded_len());
            return b;
        }

        if !self.metadata.is_empty() {
            b.push(self.metadata.len() as u8);
            let now = timestamp_ms as u32;
            for m in &self.metadata {
                for v in [
                    m.server_data_key,
                    m.first_frame_packet_arrival_time_ms,
                    m.frame_submitted_time_ms,
                    m.frame_decoded_time_ms,
                    m.frame_rendered_time_ms,
                    now, // framePacketTime
                    now, // frameDateNow
                ] {
                    b.extend_from_slice(&v.to_le_bytes());
                }
            }
        }

        if !self.gamepads.is_empty() {
            b.push(self.gamepads.len() as u8);
            for g in &self.gamepads {
                b.push(g.gamepad_index);
                b.extend_from_slice(&g.buttons.to_le_bytes());
                b.extend_from_slice(&normalize_axis(g.left_thumb_x).to_le_bytes());
                b.extend_from_slice(&normalize_axis(-g.left_thumb_y).to_le_bytes());
                b.extend_from_slice(&normalize_axis(g.right_thumb_x).to_le_bytes());
                b.extend_from_slice(&normalize_axis(-g.right_thumb_y).to_le_bytes());
                b.extend_from_slice(&normalize_trigger(g.left_trigger).to_le_bytes());
                b.extend_from_slice(&normalize_trigger(g.right_trigger).to_le_bytes());
                b.extend_from_slice(&1u32.to_le_bytes()); // PhysicalPhysicality
                b.extend_from_slice(&1u32.to_be_bytes()); // VirtualPhysicality (big endian in reference impl)
            }
        }

        if !self.mouse.is_empty() {
            b.push(self.mouse.len() as u8);
            for m in &self.mouse {
                b.extend_from_slice(&(m.x as u32).to_le_bytes());
                b.extend_from_slice(&(m.y as u32).to_le_bytes());
                b.extend_from_slice(&(m.wheel_x as u32).to_le_bytes());
                b.extend_from_slice(&(m.wheel_y as u32).to_le_bytes());
                b.push(m.buttons);
                b.push(m.relative);
            }
        }

        if !self.keyboard.is_empty() {
            b.push(self.keyboard.len() as u8);
            for k in &self.keyboard {
                b.push(2); // 1 = Known, 2 = VKey, 3 = AppCommand, 0 = Unknown
                b.push(k.pressed as u8);
                b.push(k.key_code);
            }
        }

        debug_assert_eq!(b.len(), self.encoded_len());
        b
    }
}

/// Converts a `-1.0..=1.0` axis value into the signed 16-bit wire value.
pub fn normalize_axis(value: f32) -> i16 {
    if value.is_nan() {
        return 0;
    }
    let scaled = (value * 32767.0).round();
    scaled.clamp(-32767.0, 32767.0) as i16
}

/// Converts a `0.0..=1.0` trigger value into the unsigned 16-bit wire value.
pub fn normalize_trigger(value: f32) -> u16 {
    if value.is_nan() || value <= 0.0 {
        0
    } else if value >= 1.0 {
        65535
    } else {
        (value * 65535.0) as u16
    }
}

/// Rumble command received from the server.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VibrationReport {
    pub rumble_type: u8,
    pub gamepad_index: u8,
    pub left_motor: f32,
    pub right_motor: f32,
    pub left_trigger_motor: f32,
    pub right_trigger_motor: f32,
    pub duration_ms: u16,
    pub delay_ms: u16,
    pub repeat: u8,
}

/// Messages the server sends on the input channel.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerInputReport {
    Vibration(VibrationReport),
    /// Server video dimensions.
    ServerMetadata { width: u32, height: u32 },
    Unknown(u8),
}

impl ServerInputReport {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let kind = *data.first()?;
        let u16_at = |i: usize| data.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
        let u32_at = |i: usize| {
            data.get(i..i + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        };

        match kind as u16 {
            report_type::VIBRATION => {
                if data.len() < 13 {
                    return None;
                }
                Some(Self::Vibration(VibrationReport {
                    rumble_type: data[2],
                    gamepad_index: data[3],
                    left_motor: data[4] as f32 / 100.0,
                    right_motor: data[5] as f32 / 100.0,
                    left_trigger_motor: data[6] as f32 / 100.0,
                    right_trigger_motor: data[7] as f32 / 100.0,
                    duration_ms: u16_at(8)?,
                    delay_ms: u16_at(10)?,
                    repeat: data[12],
                }))
            }
            report_type::SERVER_METADATA => {
                let height = u32_at(2)?;
                let width = u32_at(6)?;
                Some(Self::ServerMetadata { width, height })
            }
            _ => Some(Self::Unknown(kind)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn client_metadata_packet() {
        let p = InputPacket::client_metadata(0, 1);
        let b = p.to_bytes_at(0.0);
        assert_eq!(b.len(), 15);
        assert_eq!(&b[0..2], &8u16.to_le_bytes());
        assert_eq!(&b[2..6], &0u32.to_le_bytes());
        assert_eq!(b[14], 1);
    }

    #[test]
    fn gamepad_packet_layout() {
        let mut p = InputPacket::new(7);
        let mut g = GamepadFrame::new(0);
        g.set_button(buttons::A, true);
        g.set_button(buttons::NEXUS, true);
        g.left_thumb_x = 1.0;
        g.left_thumb_y = -1.0; // stick up (Gamepad API) -> +32767 on the wire
        g.right_trigger = 1.0;
        p.gamepads.push(g);

        let b = p.to_bytes_at(1234.5);
        assert_eq!(b.len(), 14 + 1 + 23);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), report_type::GAMEPAD);
        assert_eq!(u32::from_le_bytes([b[2], b[3], b[4], b[5]]), 7);
        assert_eq!(f64::from_le_bytes(b[6..14].try_into().unwrap()), 1234.5);
        assert_eq!(b[14], 1); // frame count
        assert_eq!(b[15], 0); // gamepad index
        assert_eq!(u16::from_le_bytes([b[16], b[17]]), buttons::A | buttons::NEXUS);
        assert_eq!(i16::from_le_bytes([b[18], b[19]]), 32767);
        assert_eq!(i16::from_le_bytes([b[20], b[21]]), 32767, "stick up must be positive on the wire");
        assert_eq!(u16::from_le_bytes([b[28], b[29]]), 65535);
        assert_eq!(&b[30..34], &[1, 0, 0, 0]);
        assert_eq!(&b[34..38], &[0, 0, 0, 1]);
    }

    #[test]
    fn combined_packet_len_and_flags() {
        let mut p = InputPacket::new(1);
        p.metadata.push(MetadataFrame::default());
        p.metadata.push(MetadataFrame::default());
        p.gamepads.push(GamepadFrame::new(0));
        p.keyboard.push(KeyboardFrame { pressed: true, key_code: 0x41 });
        p.mouse.push(MouseFrame::default());
        assert_eq!(
            p.report_type(),
            report_type::METADATA | report_type::GAMEPAD | report_type::MOUSE | report_type::KEYBOARD
        );
        let b = p.to_bytes_at(0.0);
        assert_eq!(b.len(), 14 + (1 + 56) + (1 + 23) + (1 + 18) + (1 + 3));
        assert_eq!(&b[b.len() - 3..], &[2, 1, 0x41]);
    }

    #[test]
    fn axis_normalisation() {
        assert_eq!(normalize_axis(0.0), 0);
        assert_eq!(normalize_axis(2.0), 32767);
        assert_eq!(normalize_axis(-2.0), -32767);
        assert_eq!(normalize_axis(f32::NAN), 0);
        assert_eq!(normalize_trigger(0.5), 32767);
        assert_eq!(normalize_trigger(-1.0), 0);
    }

    #[test]
    fn parse_vibration() {
        let data = [128u8, 0, 0, 2, 50, 100, 0, 25, 0xE8, 0x03, 10, 0, 3];
        let r = ServerInputReport::parse(&data).unwrap();
        assert_eq!(
            r,
            ServerInputReport::Vibration(VibrationReport {
                rumble_type: 0,
                gamepad_index: 2,
                left_motor: 0.5,
                right_motor: 1.0,
                left_trigger_motor: 0.0,
                right_trigger_motor: 0.25,
                duration_ms: 1000,
                delay_ms: 10,
                repeat: 3,
            })
        );
    }

    #[test]
    fn parse_server_metadata() {
        let mut data = vec![16u8, 0];
        data.extend_from_slice(&1080u32.to_le_bytes());
        data.extend_from_slice(&1920u32.to_le_bytes());
        assert_eq!(
            ServerInputReport::parse(&data),
            Some(ServerInputReport::ServerMetadata { width: 1920, height: 1080 })
        );
    }

    #[test]
    fn merge_frames() {
        let mut a = GamepadFrame::new(0);
        a.left_thumb_x = 0.2;
        a.set_button(buttons::A, true);
        let mut b = GamepadFrame::new(0);
        b.left_thumb_x = -0.9;
        b.left_trigger = 0.4;
        b.set_button(buttons::B, true);
        let m = a.merge(&b);
        assert_eq!(m.buttons, buttons::A | buttons::B);
        assert_eq!(m.left_thumb_x, -0.9);
        assert_eq!(m.left_trigger, 0.4);
    }
}
