//! Stream session lifecycle: provisioning, signalling and the run loop.
//!
//! Port of Greenlight's `StreamManager.monitorSession()` plus the stream
//! page's SDP/ICE exchange, keepalive and input loops.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use gst_webrtc::WebRTCPeerConnectionState;
use tokio::sync::mpsc;

use crate::api::xcloud::{StreamKind, XCloudApi};
use crate::auth::Msal;
use crate::channels::{ClientConfig, Protocol, ProtocolEvent};
use crate::input::keyboard::{self, ClientAction, Mapped};
use crate::input::packet::{buttons, now_ms, GamepadFrame, MetadataFrame};
use crate::input::{gamepad, InputState};
use crate::player::{Player, PlayerConfig, PlayerEvent};

/// Progress and status updates from a running session (for the CLI or GUI).
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Status(String),
    Queued { estimated_wait_secs: Option<i64> },
    Connected,
    /// Data channels are up and input is flowing.
    StreamStarted,
    VideoSize { width: u32, height: u32 },
    Title(String),
    Dialog { title: String, text: String },
    Stats(StreamStats),
    /// The session is over (normally or with an error message).
    Ended { error: Option<String> },
}

/// Requests from the UI to a running session.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    Disconnect,
    /// Tap the Xbox (Nexus) button.
    PressNexus,
    /// Keyboard key (GStreamer/GDK key name) pressed or released.
    Key { name: String, pressed: bool },
    RequestKeyframe,
    SetStats(bool),
}

/// Once-per-second stream statistics. `None` = not available.
#[derive(Debug, Clone, Default)]
pub struct StreamStats {
    pub video_mbps: f64,
    pub fps: u64,
    pub video_size: Option<(u32, u32)>,
    pub packets_lost: Option<i64>,
    pub jitter_ms: Option<f64>,
    pub rtt_ms: Option<f64>,
    pub decoder: Option<String>,
    pub jitter_buffer_ms: u32,
}

pub type EventSender = mpsc::UnboundedSender<SessionEvent>;

/// One-line human-readable stats (CLI output and overlay).
pub fn format_stats(s: &StreamStats) -> String {
    let opt = |v: Option<f64>, unit: &str| v.map(|v| format!("{v:.0} {unit}")).unwrap_or_else(|| "n/a".into());
    format!(
        "{}  {} fps  {:.1} Mbit/s  RTT {}  jitter {}  lost {}  buffer {} ms  decoder {}",
        s.video_size.map(|(w, h)| format!("{w}x{h}")).unwrap_or_else(|| "?".into()),
        s.fps,
        s.video_mbps,
        opt(s.rtt_ms, "ms"),
        opt(s.jitter_ms, "ms"),
        s.packets_lost.map(|l| l.to_string()).unwrap_or_else(|| "n/a".into()),
        s.jitter_buffer_ms,
        s.decoder.as_deref().unwrap_or("?"),
    )
}

fn emit(events: &EventSender, ev: SessionEvent) {
    let _ = events.send(ev);
}

pub struct SessionOptions {
    pub player: PlayerConfig,
    pub client: ClientConfig,
    pub gamepad: bool,
    pub rumble: bool,
    pub deadzone: f32,
    /// Request a keyframe every N seconds (0 = never).
    pub keyframe_interval: u64,
    /// Stop the remote session when the client exits.
    pub stop_on_exit: bool,
}

/// Polls `/state` until the session is `Provisioned`.
pub async fn wait_provisioned(
    api: &XCloudApi,
    msal: &Msal,
    session_id: &str,
    target: &str,
    events: &EventSender,
) -> Result<()> {
    let mut connected = false;
    let mut queued = false;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let state = match api.state(session_id).await {
            Ok(s) => s,
            Err(e) => {
                if let Some(he) = e.downcast_ref::<crate::http::HttpError>() {
                    if he.status == reqwest::StatusCode::NOT_FOUND {
                        bail!("session {session_id} no longer exists on the server");
                    }
                }
                tracing::warn!("checking session state: {e:#}");
                continue;
            }
        };
        tracing::debug!("session state: {}", state.state);
        match state.state.as_str() {
            "Provisioned" => return Ok(()),
            "Provisioning" => {}
            "ReadyToConnect" => {
                if connected {
                    continue;
                }
                emit(events, SessionEvent::Status("Authorizing console connection...".into()));
                let token = msal.msal_token().await.context("console transfer token")?;
                api.connect(session_id, &token.lpt).await.context("sending /connect")?;
                connected = true;
            }
            "WaitingForResources" => {
                if !queued {
                    queued = true;
                    let estimated_wait_secs = api
                        .waiting_times(target)
                        .await
                        .ok()
                        .and_then(|w| w.get("estimatedTotalWaitTimeInSeconds").and_then(|v| v.as_i64()));
                    emit(events, SessionEvent::Queued { estimated_wait_secs });
                }
            }
            "Failed" => {
                let details = state.error_details.map(|d| {
                    format!("{} ({})", d.message.unwrap_or_default(), d.code.map(|c| c.to_string()).unwrap_or_default())
                });
                bail!("session failed: {}", details.unwrap_or_else(|| "unknown error".into()));
            }
            other => tracing::info!("unknown session state: {other}"),
        }
    }
}

/// Runs a stream session until it ends, the server disconnects, or a
/// [`SessionCommand::Disconnect`] arrives. Always emits a final
/// [`SessionEvent::Ended`].
pub async fn run(
    api: XCloudApi,
    msal: Arc<Msal>,
    target: &str,
    opts: SessionOptions,
    events: EventSender,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
) -> Result<()> {
    emit(&events, SessionEvent::Status(format!("Starting {} stream...", api.kind().as_str())));
    let result = async {
        let play = api.start_stream(target).await?;
        let session_id = play.id();
        tracing::info!("session {session_id} ({})", play.session_path);
        let r = run_session(&api, &msal, &session_id, target, &opts, &events, &mut commands).await;
        if opts.stop_on_exit {
            if let Err(e) = api.stop_stream(&session_id).await {
                tracing::debug!("stopping session: {e:#}");
            }
        }
        r
    }
    .await;
    emit(&events, SessionEvent::Ended { error: result.as_ref().err().map(|e| format!("{e:#}")) });
    result
}

async fn run_session(
    api: &XCloudApi,
    msal: &Msal,
    session_id: &str,
    target: &str,
    opts: &SessionOptions,
    events: &EventSender,
    commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
) -> Result<()> {
    emit(events, SessionEvent::Status("Waiting for the console...".into()));
    loop {
        tokio::select! {
            r = wait_provisioned(api, msal, session_id, target, events) => { r?; break; }
            cmd = commands.recv() => match cmd {
                Some(SessionCommand::Disconnect) | None => bail!("cancelled"),
                Some(_) => continue,
            },
        }
    }
    emit(events, SessionEvent::Status("Connecting...".into()));

    // Like the web client: fetch the session configuration before the SDP
    // exchange. Use its STUN server unless one was given explicitly.
    let mut player_config = opts.player.clone();
    match api.configuration(session_id).await {
        Ok(cfg) => {
            tracing::debug!("session configuration: {}", redact_configuration(&cfg));
            if player_config.stun_server.is_none() {
                if let Some(stun) = cfg
                    .pointer("/serverDetails/stunServerAddresses/0")
                    .and_then(|v| v.as_str())
                {
                    player_config.stun_server = Some(stun.replacen("stun:", "stun://", 1));
                }
            }
        }
        Err(e) => tracing::warn!("fetching session configuration: {e:#}"),
    }

    let (tx, mut player_events) = mpsc::unbounded_channel();
    let player = Player::new(player_config, tx)?;

    let offer = player.create_offer().await?;
    tracing::debug!("local offer:\n{offer}");
    let answer = api.exchange_sdp(session_id, &offer).await?;
    tracing::debug!("remote answer:\n{}", answer.sdp);
    player.set_remote_answer(&answer.sdp).await?;

    let local = player.local_candidates(Duration::from_secs(5)).await;
    if local.is_empty() {
        bail!("no local ICE candidates gathered");
    }
    tracing::info!("sending {} local ICE candidates", local.len());
    for c in &local {
        tracing::debug!("local candidate [{}]: {}", c.sdp_mid.as_deref().unwrap_or("?"), c.candidate);
    }
    let remote = api.exchange_ice(session_id, &local).await?;
    tracing::info!("received {} remote ICE candidates", remote.len());
    player.add_remote_candidates(&remote);

    let input = InputState::default();
    let rumble = if opts.gamepad {
        gamepad::spawn(input.clone(), opts.deadzone, opts.rumble)
    } else {
        gamepad::RumbleSender::disabled()
    };

    let mut protocol = Protocol::new(opts.client.clone());
    let mut tick = tokio::time::interval(Duration::from_millis(16));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut keepalive = tokio::time::interval(Duration::from_secs(30));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let keyframe_every = (opts.keyframe_interval > 0).then(|| Duration::from_secs(opts.keyframe_interval));
    let mut last_keyframe = Instant::now();
    let mut stats_every = tokio::time::interval(Duration::from_secs(1));
    let mut show_stats = false;
    // Release time of an overlay-injected Nexus tap.
    let mut nexus_release: Option<Instant> = None;
    let mut connected = false;
    let mut last_bytes = 0u64;
    let mut last_frames = 0u64;
    let connect_secs = opts.player.ice_check_secs.max(10) as u64 + 20;
    let connect_deadline = tokio::time::sleep(Duration::from_secs(connect_secs));
    tokio::pin!(connect_deadline);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                if nexus_release.is_some_and(|t| Instant::now() >= t) {
                    input.set_overlay(GamepadFrame::default());
                    nexus_release = None;
                }
                if let Some(out) = protocol.tick(input.snapshot(), now_ms(), 33.0) {
                    player.send(&out);
                }
                if let Some(every) = keyframe_every {
                    if protocol.handshake_completed() && last_keyframe.elapsed() >= every {
                        player.send(&Protocol::keyframe_request(true));
                        last_keyframe = Instant::now();
                    }
                }
            }
            _ = keepalive.tick(), if connected => {
                if let Err(e) = api.keepalive(session_id).await {
                    tracing::warn!("keepalive failed: {e:#}");
                }
            }
            _ = stats_every.tick(), if show_stats => {
                let bytes = player.stats.video_bytes.load(Ordering::Relaxed);
                let frames = player.stats.video_frames.load(Ordering::Relaxed);
                let mut stats = player.webrtc_stats().await;
                stats.video_mbps = (bytes - last_bytes) as f64 * 8.0 / 1_000_000.0;
                stats.fps = frames - last_frames;
                stats.video_size = protocol.server_video_size;
                stats.jitter_buffer_ms = opts.player.latency_ms;
                last_bytes = bytes;
                last_frames = frames;
                emit(events, SessionEvent::Stats(stats));
            }
            cmd = commands.recv() => match cmd {
                None | Some(SessionCommand::Disconnect) => break,
                Some(SessionCommand::PressNexus) => {
                    let mut f = GamepadFrame::default();
                    f.set_button(buttons::NEXUS, true);
                    input.set_overlay(f);
                    nexus_release = Some(Instant::now() + Duration::from_millis(150));
                }
                Some(SessionCommand::Key { name, pressed }) => apply_key(&input, &name, pressed),
                Some(SessionCommand::RequestKeyframe) => player.send(&Protocol::keyframe_request(true)),
                Some(SessionCommand::SetStats(on)) => {
                    show_stats = on;
                    last_bytes = player.stats.video_bytes.load(Ordering::Relaxed);
                    last_frames = player.stats.video_frames.load(Ordering::Relaxed);
                }
            },
            _ = &mut connect_deadline, if !connected => {
                bail!("WebRTC connection was not established within {connect_secs} seconds");
            }
            ev = player_events.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    PlayerEvent::ChannelOpen(kind) => {
                        for out in protocol.on_open(kind) {
                            player.send(&out);
                        }
                    }
                    PlayerEvent::ChannelClosed(kind) => {
                        tracing::info!("data channel `{}` closed", kind.label());
                        if kind == crate::channels::ChannelKind::Input && connected {
                            bail!("stream closed by the server");
                        }
                    }
                    PlayerEvent::ChannelMessage(kind, data) => {
                        let (out, evs) = protocol.on_message(kind, &data);
                        for o in &out {
                            player.send(o);
                        }
                        for e in evs {
                            match e {
                                ProtocolEvent::HandshakeCompleted => emit(events, SessionEvent::StreamStarted),
                                ProtocolEvent::Vibration(v) => rumble.send(v),
                                ProtocolEvent::ServerVideoSize { width, height } => {
                                    tracing::info!("server video size {width}x{height}");
                                    emit(events, SessionEvent::VideoSize { width, height });
                                }
                                ProtocolEvent::TitleInfo(info) => {
                                    tracing::info!("title info: {info}");
                                    if let Some(id) = info.get("titleaumid").and_then(|v| v.as_str()) {
                                        let name = id.rsplit('!').next().unwrap_or(id).to_string();
                                        emit(events, SessionEvent::Title(name));
                                    }
                                }
                                ProtocolEvent::Dialog { title, text } => emit(events, SessionEvent::Dialog { title, text }),
                                ProtocolEvent::ServerDisconnect => {
                                    tracing::info!("server ended the session");
                                    player.stop();
                                    return Ok(());
                                }
                            }
                        }
                    }
                    PlayerEvent::ConnectionState(state) => {
                        tracing::info!("peer connection state: {state:?}");
                        match state {
                            WebRTCPeerConnectionState::Connected => {
                                if !connected {
                                    emit(events, SessionEvent::Connected);
                                }
                                connected = true;
                            }
                            WebRTCPeerConnectionState::Failed => bail!("WebRTC connection failed"),
                            WebRTCPeerConnectionState::Closed if connected => break,
                            _ => {}
                        }
                    }
                    PlayerEvent::VideoFrame { rtp_timestamp, arrival_ms } => {
                        if protocol.input_started() {
                            let t = arrival_ms as u32;
                            protocol.queue_metadata(MetadataFrame {
                                server_data_key: rtp_timestamp,
                                first_frame_packet_arrival_time_ms: t,
                                frame_submitted_time_ms: t,
                                frame_decoded_time_ms: t,
                                frame_rendered_time_ms: t,
                            });
                        }
                    }
                    PlayerEvent::KeyPress(key) => {
                        if let Some(action) = keyboard::client_action(&key) {
                            match action {
                                ClientAction::ToggleStats => {
                                    show_stats = !show_stats;
                                    last_bytes = player.stats.video_bytes.load(Ordering::Relaxed);
                                    last_frames = player.stats.video_frames.load(Ordering::Relaxed);
                                }
                                ClientAction::RequestKeyframe => player.send(&Protocol::keyframe_request(true)),
                                ClientAction::Quit => break,
                            }
                        } else {
                            apply_key(&input, &key, true);
                        }
                    }
                    PlayerEvent::KeyRelease(key) => apply_key(&input, &key, false),
                    PlayerEvent::Closed => break,
                    PlayerEvent::Error(e) => return Err(anyhow!("player error: {e}")),
                }
            }
        }
    }

    player.stop();
    Ok(())
}

/// The configuration includes an SRTP key; keep it out of logs.
fn redact_configuration(cfg: &serde_json::Value) -> serde_json::Value {
    let mut c = cfg.clone();
    if let Some(srtp) = c.pointer_mut("/serverDetails/srtp") {
        *srtp = serde_json::json!("<redacted>");
    }
    c
}

fn apply_key(input: &InputState, key: &str, pressed: bool) {
    let Some(mapped) = keyboard::map_key(key) else { return };
    input.update_keyboard(|f| match mapped {
        Mapped::Button(mask) => f.set_button(mask, pressed),
        Mapped::LeftTrigger => f.left_trigger = if pressed { 1.0 } else { 0.0 },
        Mapped::RightTrigger => f.right_trigger = if pressed { 1.0 } else { 0.0 },
    });
}

pub fn kind_from_str(s: &str) -> Result<StreamKind> {
    match s {
        "home" => Ok(StreamKind::Home),
        "cloud" => Ok(StreamKind::Cloud),
        _ => Err(anyhow!("unknown stream type {s}")),
    }
}
