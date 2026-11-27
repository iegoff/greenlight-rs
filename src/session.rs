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
use crate::input::packet::{now_ms, MetadataFrame};
use crate::input::{gamepad, InputState};
use crate::player::{Player, PlayerConfig, PlayerEvent};

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
pub async fn wait_provisioned(api: &XCloudApi, msal: &Msal, session_id: &str, target: &str) -> Result<()> {
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
                eprintln!("Authorizing console connection...");
                let token = msal.msal_token().await.context("console transfer token")?;
                api.connect(session_id, &token.lpt).await.context("sending /connect")?;
                connected = true;
            }
            "WaitingForResources" => {
                if !queued {
                    queued = true;
                    match api.waiting_times(target).await {
                        Ok(w) => eprintln!("In queue. Estimated wait: {}s", w
                            .get("estimatedTotalWaitTimeInSeconds")
                            .and_then(|v| v.as_i64())
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".into())),
                        Err(_) => eprintln!("In queue, waiting for a server..."),
                    }
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

pub async fn run(api: XCloudApi, msal: Arc<Msal>, target: &str, opts: SessionOptions) -> Result<()> {
    eprintln!("Starting {} stream for {target}...", api.kind().as_str());
    let play = api.start_stream(target).await?;
    let session_id = play.id();
    tracing::info!("session {session_id} ({})", play.session_path);

    let result = run_session(&api, &msal, &session_id, target, &opts).await;

    if opts.stop_on_exit {
        if let Err(e) = api.stop_stream(&session_id).await {
            tracing::debug!("stopping session: {e:#}");
        }
    }
    result
}

async fn run_session(
    api: &XCloudApi,
    msal: &Msal,
    session_id: &str,
    target: &str,
    opts: &SessionOptions,
) -> Result<()> {
    tokio::select! {
        r = wait_provisioned(api, msal, session_id, target) => r?,
        _ = tokio::signal::ctrl_c() => bail!("interrupted"),
    }
    eprintln!("Session provisioned, negotiating WebRTC...");

    let (tx, mut events) = mpsc::unbounded_channel();
    let player = Player::new(opts.player.clone(), tx)?;

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
    let mut connected = false;
    let mut last_bytes = 0u64;
    let mut last_frames = 0u64;
    let connect_secs = opts.player.ice_check_secs as u64 + 20;
    let connect_deadline = tokio::time::sleep(Duration::from_secs(connect_secs));
    tokio::pin!(connect_deadline);

    loop {
        tokio::select! {
            _ = tick.tick() => {
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
                eprintln!(
                    "[stats] video {:.2} Mbit/s, {} fps, server size {:?}",
                    (bytes - last_bytes) as f64 * 8.0 / 1_000_000.0,
                    frames - last_frames,
                    protocol.server_video_size
                );
                last_bytes = bytes;
                last_frames = frames;
            }
            _ = &mut connect_deadline, if !connected => {
                bail!("WebRTC connection was not established within {connect_secs} seconds");
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("Stopping stream...");
                break;
            }
            ev = events.recv() => {
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
                            eprintln!("Stream closed by the server.");
                            break;
                        }
                    }
                    PlayerEvent::ChannelMessage(kind, data) => {
                        let (out, evs) = protocol.on_message(kind, &data);
                        for o in &out {
                            player.send(o);
                        }
                        for e in evs {
                            match e {
                                ProtocolEvent::HandshakeCompleted => eprintln!("Stream started. Press ~ for stats, F12 to quit."),
                                ProtocolEvent::Vibration(v) => rumble.send(v),
                                ProtocolEvent::ServerVideoSize { width, height } => {
                                    tracing::info!("server video size {width}x{height}");
                                }
                                ProtocolEvent::TitleInfo(info) => tracing::info!("title info: {info}"),
                                ProtocolEvent::Dialog { title, text } => eprintln!("[console] {title}: {text}"),
                                ProtocolEvent::ServerDisconnect => {
                                    eprintln!("Server ended the session.");
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
                                    eprintln!("Connected.");
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
                                ClientAction::ToggleStats => show_stats = !show_stats,
                                ClientAction::RequestKeyframe => player.send(&Protocol::keyframe_request(true)),
                                ClientAction::Quit => break,
                            }
                        } else {
                            apply_key(&input, &key, true);
                        }
                    }
                    PlayerEvent::KeyRelease(key) => apply_key(&input, &key, false),
                    PlayerEvent::Closed => {
                        eprintln!("Window closed.");
                        break;
                    }
                    PlayerEvent::Error(e) => return Err(anyhow!("player error: {e}")),
                }
            }
        }
    }

    player.stop();
    Ok(())
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
