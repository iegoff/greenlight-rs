//! Protocol logic for the four xCloud data channels, independent of the
//! WebRTC transport so it can be unit tested.
//!
//! * `message` (`messageV1`): JSON handshake, client configuration and
//!   server transactions (title info, dialogs, disconnects).
//! * `control` (`controlV1`): authorization, gamepad hot-plug, keyframes.
//! * `input` (`1.0`): binary input reports (see [`crate::input::packet`]).
//! * `chat` (`chatV1`): voice chat (unused, but must be negotiated).

use serde_json::{json, Value};
use uuid::Uuid;

use crate::input::packet::{
    GamepadFrame, InputPacket, KeyboardFrame, MetadataFrame, MouseFrame, ServerInputReport,
    VibrationReport,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    Chat,
    Control,
    Input,
    Message,
}

impl ChannelKind {
    pub const ALL: [ChannelKind; 4] =
        [ChannelKind::Chat, ChannelKind::Control, ChannelKind::Input, ChannelKind::Message];

    pub fn label(self) -> &'static str {
        match self {
            ChannelKind::Chat => "chat",
            ChannelKind::Control => "control",
            ChannelKind::Input => "input",
            ChannelKind::Message => "message",
        }
    }

    pub fn protocol(self) -> &'static str {
        match self {
            ChannelKind::Chat => "chatV1",
            ChannelKind::Control => "controlV1",
            ChannelKind::Input => "1.0",
            ChannelKind::Message => "messageV1",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.label() == label)
    }
}

/// Something the protocol wants the transport to send.
#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing {
    pub channel: ChannelKind,
    pub data: Vec<u8>,
}

/// Events surfaced to the application.
#[derive(Debug, Clone, PartialEq)]
pub enum ProtocolEvent {
    HandshakeCompleted,
    Vibration(VibrationReport),
    ServerVideoSize { width: u32, height: u32 },
    TitleInfo(Value),
    /// A system dialog the server wants shown. Answered automatically with `Result: 0`.
    Dialog { title: String, text: String },
    ServerDisconnect,
}

/// Client configuration sent after the message-channel handshake.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub width: u32,
    pub height: u32,
    pub system_uis: Vec<i32>,
    pub system_ui_version: [u32; 3],
    pub max_touchpoints: u8,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self { width: 1920, height: 1080, system_uis: vec![], system_ui_version: [0, 2, 0], max_touchpoints: 1 }
    }
}

pub struct Protocol {
    config: ClientConfig,
    handshake_completed: bool,
    input_started: bool,
    sequence: u32,
    metadata: Vec<MetadataFrame>,
    mouse: Vec<MouseFrame>,
    keyboard: Vec<KeyboardFrame>,
    last_gamepad: Option<GamepadFrame>,
    last_gamepad_sent_ms: f64,
    pub server_video_size: Option<(u32, u32)>,
}

impl Protocol {
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            handshake_completed: false,
            input_started: false,
            sequence: 0,
            metadata: Vec::new(),
            mouse: Vec::new(),
            keyboard: Vec::new(),
            last_gamepad: None,
            last_gamepad_sent_ms: f64::MIN,
            server_video_size: None,
        }
    }

    pub fn handshake_completed(&self) -> bool {
        self.handshake_completed
    }

    pub fn input_started(&self) -> bool {
        self.input_started
    }

    fn json(channel: ChannelKind, v: Value) -> Outgoing {
        Outgoing { channel, data: v.to_string().into_bytes() }
    }

    pub fn on_open(&mut self, channel: ChannelKind) -> Vec<Outgoing> {
        match channel {
            ChannelKind::Message => vec![Self::json(
                ChannelKind::Message,
                json!({
                    "type": "Handshake",
                    "version": "messageV1",
                    "id": Uuid::new_v4().to_string(),
                    "cv": "0",
                }),
            )],
            _ => vec![],
        }
    }

    pub fn on_message(&mut self, channel: ChannelKind, data: &[u8]) -> (Vec<Outgoing>, Vec<ProtocolEvent>) {
        match channel {
            ChannelKind::Message => self.on_message_channel(data),
            ChannelKind::Input => {
                let mut events = vec![];
                match ServerInputReport::parse(data) {
                    Some(ServerInputReport::Vibration(v)) => events.push(ProtocolEvent::Vibration(v)),
                    Some(ServerInputReport::ServerMetadata { width, height }) => {
                        self.server_video_size = Some((width, height));
                        events.push(ProtocolEvent::ServerVideoSize { width, height });
                    }
                    _ => tracing::debug!("input: unhandled report {:?}", data.first()),
                }
                (vec![], events)
            }
            ChannelKind::Control | ChannelKind::Chat => {
                tracing::debug!("{}: {}", channel.label(), String::from_utf8_lossy(data));
                (vec![], vec![])
            }
        }
    }

    fn on_message_channel(&mut self, data: &[u8]) -> (Vec<Outgoing>, Vec<ProtocolEvent>) {
        let Ok(msg) = serde_json::from_slice::<Value>(data) else {
            tracing::warn!("message: invalid JSON: {}", String::from_utf8_lossy(data));
            return (vec![], vec![]);
        };
        let mut out = vec![];
        let mut events = vec![];
        match msg.get("type").and_then(Value::as_str).unwrap_or_default() {
            "HandshakeAck" => {
                self.handshake_completed = true;
                out.extend(self.control_authorization());
                out.push(self.start_input());
                out.extend(self.client_configuration());
                events.push(ProtocolEvent::HandshakeCompleted);
            }
            "TransactionStart" | "Message" => {
                let target = msg.get("target").and_then(Value::as_str).unwrap_or_default();
                let id = msg.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                let content: Value = msg
                    .get("content")
                    .and_then(Value::as_str)
                    .and_then(|c| serde_json::from_str(c).ok())
                    .unwrap_or(Value::Null);
                match target {
                    "/streaming/sessionLifetimeManagement/serverInitiatedDisconnect" => {
                        out.push(Self::transaction_complete(&id, json!("")));
                        events.push(ProtocolEvent::ServerDisconnect);
                    }
                    "/streaming/properties/titleinfo" => events.push(ProtocolEvent::TitleInfo(content)),
                    "/streaming/systemUi/messages/ShowMessageDialog" => {
                        let s = |k: &str| content.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                        events.push(ProtocolEvent::Dialog { title: s("TitleText"), text: s("ContentText") });
                        out.push(Self::transaction_complete(&id, json!({ "Result": 0 })));
                    }
                    other => tracing::debug!("message: unhandled target {other}: {content}"),
                }
            }
            other => tracing::debug!("message: unhandled type {other}: {msg}"),
        }
        (out, events)
    }

    fn transaction_complete(id: &str, content: Value) -> Outgoing {
        Self::json(
            ChannelKind::Message,
            json!({ "type": "TransactionComplete", "content": content.to_string(), "id": id, "cv": "" }),
        )
    }

    fn message(target: &str, content: Value) -> Outgoing {
        Self::json(
            ChannelKind::Message,
            json!({
                "type": "Message",
                "content": content.to_string(),
                "id": Uuid::new_v4().to_string(),
                "target": target,
                "cv": "",
            }),
        )
    }

    fn control_authorization(&self) -> Vec<Outgoing> {
        vec![
            Self::json(
                ChannelKind::Control,
                json!({ "message": "authorizationRequest", "accessKey": "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E" }),
            ),
            Self::gamepad_changed(0, true),
        ]
    }

    pub fn gamepad_changed(index: u8, was_added: bool) -> Outgoing {
        Self::json(
            ChannelKind::Control,
            json!({ "message": "gamepadChanged", "gamepadIndex": index, "wasAdded": was_added }),
        )
    }

    pub fn keyframe_request(ifr_requested: bool) -> Outgoing {
        Self::json(
            ChannelKind::Control,
            json!({ "message": "videoKeyframeRequested", "ifrRequested": ifr_requested }),
        )
    }

    fn start_input(&mut self) -> Outgoing {
        self.input_started = true;
        let p = InputPacket::client_metadata(self.sequence, self.config.max_touchpoints);
        Outgoing { channel: ChannelKind::Input, data: p.to_bytes() }
    }

    fn client_configuration(&self) -> Vec<Outgoing> {
        let (w, h) = (self.config.width, self.config.height);
        vec![
            Self::message(
                "/streaming/systemUi/configuration",
                json!({ "version": self.config.system_ui_version, "systemUis": self.config.system_uis }),
            ),
            Self::message(
                "/streaming/properties/clientappinstallidchanged",
                json!({ "clientAppInstallId": Uuid::new_v4().to_string() }),
            ),
            Self::message("/streaming/characteristics/orientationchanged", json!({ "orientation": 0 })),
            Self::message(
                "/streaming/characteristics/touchinputenabledchanged",
                json!({ "touchInputEnabled": false }),
            ),
            Self::message("/streaming/characteristics/clientdevicecapabilities", json!({})),
            Self::message(
                "/streaming/characteristics/dimensionschanged",
                json!({
                    "horizontal": w, "vertical": h,
                    "preferredWidth": w, "preferredHeight": h,
                    "safeAreaLeft": 0, "safeAreaTop": 0,
                    "safeAreaRight": w, "safeAreaBottom": h,
                    "supportsCustomResolution": true,
                }),
            ),
        ]
    }

    pub fn queue_metadata(&mut self, frame: MetadataFrame) {
        // Avoid unbounded growth if the input channel is not open yet.
        if self.metadata.len() < 64 {
            self.metadata.push(frame);
        }
    }

    pub fn queue_mouse(&mut self, frame: MouseFrame) {
        self.mouse.push(frame);
    }

    pub fn queue_keyboard(&mut self, frame: KeyboardFrame) {
        self.keyboard.push(frame);
    }

    /// Called every ~16 ms. Builds an input report from the current gamepad
    /// state and any queued frames. The gamepad is sent when it changes and
    /// at least every `heartbeat_ms` otherwise.
    pub fn tick(&mut self, gamepad: GamepadFrame, now_ms: f64, heartbeat_ms: f64) -> Option<Outgoing> {
        if !self.input_started {
            return None;
        }
        let changed = self.last_gamepad != Some(gamepad);
        let due = now_ms - self.last_gamepad_sent_ms >= heartbeat_ms;

        let mut packet = InputPacket::new(0);
        if changed || due {
            packet.gamepads.push(gamepad);
            self.last_gamepad = Some(gamepad);
            self.last_gamepad_sent_ms = now_ms;
        }
        packet.metadata = std::mem::take(&mut self.metadata);
        packet.mouse = std::mem::take(&mut self.mouse);
        packet.keyboard = std::mem::take(&mut self.keyboard);
        if packet.is_empty() {
            return None;
        }
        self.sequence = self.sequence.wrapping_add(1);
        packet.sequence = self.sequence;
        Some(Outgoing { channel: ChannelKind::Input, data: packet.to_bytes_at(now_ms) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::packet::{buttons, report_type};

    fn parse(o: &Outgoing) -> Value {
        serde_json::from_slice(&o.data).unwrap()
    }

    #[test]
    fn handshake_flow() {
        let mut p = Protocol::new(ClientConfig::default());
        assert!(p.on_open(ChannelKind::Input).is_empty());
        let hs = p.on_open(ChannelKind::Message);
        assert_eq!(parse(&hs[0])["type"], "Handshake");
        assert_eq!(parse(&hs[0])["version"], "messageV1");

        // No input before the handshake.
        assert!(p.tick(GamepadFrame::new(0), 0.0, 33.0).is_none());

        let (out, events) = p.on_message(ChannelKind::Message, br#"{"type":"HandshakeAck","version":"messageV1"}"#);
        assert_eq!(events, vec![ProtocolEvent::HandshakeCompleted]);
        assert!(p.handshake_completed());

        let control: Vec<_> = out.iter().filter(|o| o.channel == ChannelKind::Control).map(parse).collect();
        assert_eq!(control[0]["message"], "authorizationRequest");
        assert_eq!(control[1]["message"], "gamepadChanged");
        assert_eq!(control[1]["wasAdded"], true);

        let input: Vec<_> = out.iter().filter(|o| o.channel == ChannelKind::Input).collect();
        assert_eq!(input.len(), 1);
        assert_eq!(u16::from_le_bytes([input[0].data[0], input[0].data[1]]), report_type::CLIENT_METADATA);

        let targets: Vec<String> = out
            .iter()
            .filter(|o| o.channel == ChannelKind::Message)
            .map(|o| parse(o)["target"].as_str().unwrap().to_string())
            .collect();
        assert!(targets.contains(&"/streaming/characteristics/dimensionschanged".to_string()));
        assert!(targets.contains(&"/streaming/systemUi/configuration".to_string()));
    }

    #[test]
    fn tick_sends_on_change_and_heartbeat() {
        let mut p = Protocol::new(ClientConfig::default());
        p.on_message(ChannelKind::Message, br#"{"type":"HandshakeAck"}"#);

        let idle = GamepadFrame::new(0);
        assert!(p.tick(idle, 0.0, 33.0).is_some()); // first frame
        assert!(p.tick(idle, 16.0, 33.0).is_none()); // unchanged, not due
        assert!(p.tick(idle, 40.0, 33.0).is_some()); // heartbeat

        let mut pressed = idle;
        pressed.set_button(buttons::A, true);
        let o = p.tick(pressed, 41.0, 33.0).unwrap();
        assert_eq!(u16::from_le_bytes([o.data[0], o.data[1]]), report_type::GAMEPAD);
        assert_eq!(u32::from_le_bytes([o.data[2], o.data[3], o.data[4], o.data[5]]), 3);
    }

    #[test]
    fn disconnect_and_dialog() {
        let mut p = Protocol::new(ClientConfig::default());
        let (out, ev) = p.on_message(
            ChannelKind::Message,
            br#"{"type":"TransactionStart","id":"t1","target":"/streaming/sessionLifetimeManagement/serverInitiatedDisconnect","content":"{}"}"#,
        );
        assert_eq!(ev, vec![ProtocolEvent::ServerDisconnect]);
        assert_eq!(parse(&out[0])["type"], "TransactionComplete");
        assert_eq!(parse(&out[0])["id"], "t1");

        let (out, ev) = p.on_message(
            ChannelKind::Message,
            br#"{"type":"TransactionStart","id":"t2","target":"/streaming/systemUi/messages/ShowMessageDialog","content":"{\"TitleText\":\"Hi\",\"ContentText\":\"There\"}"}"#,
        );
        assert_eq!(ev, vec![ProtocolEvent::Dialog { title: "Hi".into(), text: "There".into() }]);
        assert_eq!(parse(&out[0])["content"], "{\"Result\":0}");
    }
}
