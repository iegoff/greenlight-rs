//! WebRTC media player built on GStreamer's `webrtcbin`.
//!
//! Replaces the browser `RTCPeerConnection` used by Greenlight:
//!
//! * one `sendrecv` Opus audio transceiver and one `recvonly` H.264 video transceiver,
//! * the four negotiated data channels (`chat`, `control`, `input`, `message`),
//! * incoming RTP is decoded with `decodebin` and rendered with auto sinks,
//! * RTP timestamps of completed video frames are reported (for the input
//!   channel's frame metadata), and keyboard events are captured from the
//!   video window through GStreamer navigation events.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use gst::prelude::*;
use gst_webrtc::{
    WebRTCDataChannel, WebRTCICEGatheringState, WebRTCPeerConnectionState, WebRTCRTPTransceiverDirection,
    WebRTCSDPType, WebRTCSessionDescription,
};
use tokio::sync::{mpsc, oneshot, watch};

use crate::channels::{ChannelKind, Outgoing};
use crate::sdp::{self, IceCandidate};

/// H.264 profile, in Greenlight's preference terminology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum H264Profile {
    /// Main profile (`4d`), Greenlight's "high" quality.
    High,
    /// Constrained baseline (`42e`).
    Medium,
    /// Baseline (`420`).
    Low,
}

impl H264Profile {
    fn profile_level_id(self) -> &'static str {
        match self {
            H264Profile::High => "4d001f",
            H264Profile::Medium => "42e01f",
            H264Profile::Low => "42001f",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlayerConfig {
    /// Codec preference order. Defaults to high, medium, low.
    pub profiles: Vec<H264Profile>,
    pub stereo: bool,
    pub video_kbps: u32,
    pub audio_kbps: u32,
    pub stun_server: Option<String>,
    /// Jitter-buffer latency in milliseconds.
    pub latency_ms: u32,
    /// Custom video sink description (e.g. `glimagesink`), default `autovideosink`.
    pub video_sink: Option<String>,
    pub audio_sink: Option<String>,
    /// Render frames as soon as they are decoded (no A/V sync).
    pub low_latency: bool,
    pub audio: bool,
    /// How long (roughly, in seconds) ICE keeps sending connectivity checks
    /// on each candidate pair before giving up. libnice's default is ~2 s;
    /// browsers keep trying for ~30 s, which matters when the console's
    /// checks (which open its NAT) start a few seconds after ours.
    pub ice_check_secs: u32,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            profiles: vec![H264Profile::High, H264Profile::Medium, H264Profile::Low],
            stereo: true,
            video_kbps: 0,
            audio_kbps: 0,
            stun_server: None,
            latency_ms: 100,
            video_sink: None,
            audio_sink: None,
            low_latency: false,
            audio: true,
            ice_check_secs: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    ChannelOpen(ChannelKind),
    ChannelClosed(ChannelKind),
    ChannelMessage(ChannelKind, Vec<u8>),
    ConnectionState(WebRTCPeerConnectionState),
    /// A complete video frame arrived (RTP timestamp, arrival time ms).
    VideoFrame { rtp_timestamp: u32, arrival_ms: f64 },
    KeyPress(String),
    KeyRelease(String),
    /// Video window was closed or the pipeline ended.
    Closed,
    Error(String),
}

#[derive(Default)]
pub struct Stats {
    pub video_bytes: AtomicU64,
    pub video_frames: AtomicU64,
    pub audio_bytes: AtomicU64,
}

pub struct Player {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    config: PlayerConfig,
    channels: Arc<Mutex<HashMap<ChannelKind, WebRTCDataChannel>>>,
    candidates: Arc<Mutex<Vec<IceCandidate>>>,
    gathering: watch::Receiver<WebRTCICEGatheringState>,
    /// `(webrtcbin mid, browser-style mid)` for each m-line of the local offer.
    mid_map: Mutex<Vec<(String, String)>>,
    pub stats: Arc<Stats>,
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn promise() -> (gst::Promise, oneshot::Receiver<Result<Option<gst::Structure>>>) {
    let (tx, rx) = oneshot::channel();
    let p = gst::Promise::with_change_func(move |reply| {
        let r = match reply {
            Ok(s) => Ok(s.map(|s| s.to_owned())),
            Err(e) => Err(anyhow!("promise failed: {e:?}")),
        };
        let _ = tx.send(r);
    });
    (p, rx)
}

impl Player {
    pub fn new(config: PlayerConfig, events: mpsc::UnboundedSender<PlayerEvent>) -> Result<Self> {
        gst::init()?;
        for el in ["webrtcbin", "decodebin", "videoconvert", "audioconvert", "audioresample"] {
            if gst::ElementFactory::find(el).is_none() {
                bail!("GStreamer element `{el}` not found; install gst-plugins-base/good/bad and libnice");
            }
        }

        let pipeline = gst::Pipeline::with_name("greenlight");
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .name("webrtc")
            // Browser parity: Greenlight uses `new RTCPeerConnection({})` (bundle policy
            // "balanced"), which never emits port-0 `a=bundle-only` m-lines. max-compat
            // keeps every m-line on a real port while still offering a BUNDLE group.
            .property_from_str("bundle-policy", "max-compat")
            .property("latency", config.latency_ms)
            .build()?;
        if let Some(stun) = &config.stun_server {
            webrtc.set_property("stun-server", stun);
        }
        configure_ice_timing(&webrtc, config.ice_check_secs);
        pipeline.add(&webrtc)?;

        let stats = Arc::new(Stats::default());
        let channels: Arc<Mutex<HashMap<ChannelKind, WebRTCDataChannel>>> = Default::default();
        let candidates: Arc<Mutex<Vec<IceCandidate>>> = Default::default();
        let (gather_tx, gathering) = watch::channel(WebRTCICEGatheringState::New);

        // --- transceivers -------------------------------------------------
        let audio_caps: gst::Caps = format!(
            "application/x-rtp,media=audio,encoding-name=OPUS,clock-rate=48000,payload=111,\
             encoding-params=(string)2,minptime=(string)10,useinbandfec=(string)1{}",
            if config.stereo { ",stereo=(string)1" } else { "" }
        )
        .parse()
        .context("audio caps")?;
        webrtc.emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>(
            "add-transceiver",
            &[&WebRTCRTPTransceiverDirection::Sendrecv, &audio_caps],
        );

        let mut video_caps = gst::Caps::new_empty();
        {
            let caps = video_caps.get_mut().unwrap();
            for (i, profile) in config.profiles.iter().enumerate() {
                caps.append_structure(
                    gst::Structure::builder("application/x-rtp")
                        .field("media", "video")
                        .field("encoding-name", "H264")
                        .field("clock-rate", 90000i32)
                        .field("payload", 102i32 + i as i32)
                        .field("packetization-mode", "1")
                        .field("level-asymmetry-allowed", "1")
                        .field("profile-level-id", profile.profile_level_id())
                        .field("rtcp-fb-nack", true)
                        .field("rtcp-fb-nack-pli", true)
                        .field("rtcp-fb-ccm-fir", true)
                        .field("rtcp-fb-transport-cc", true)
                        .build(),
                );
            }
        }
        let video_tr = webrtc.emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>(
            "add-transceiver",
            &[&WebRTCRTPTransceiverDirection::Recvonly, &video_caps],
        );
        video_tr.set_property("do-nack", true);

        // --- signals ------------------------------------------------------
        {
            let candidates = candidates.clone();
            webrtc.connect("on-ice-candidate", false, move |values| {
                let mline = values[1].get::<u32>().unwrap_or(0);
                let candidate = values[2].get::<String>().unwrap_or_default();
                if !candidate.is_empty() {
                    candidates.lock().unwrap().push(IceCandidate {
                        // Bare `candidate:...` like a browser's RTCIceCandidate.candidate.
                        candidate,
                        sdp_m_line_index: Some(mline),
                        sdp_mid: Some(mline.to_string()),
                        message_type: None,
                    });
                }
                None
            });
        }
        webrtc.connect_notify(Some("ice-gathering-state"), move |w, _| {
            let s = w.property::<WebRTCICEGatheringState>("ice-gathering-state");
            tracing::debug!("ICE gathering state: {s:?}");
            let _ = gather_tx.send(s);
        });
        {
            let events = events.clone();
            webrtc.connect_notify(Some("connection-state"), move |w, _| {
                let s = w.property::<WebRTCPeerConnectionState>("connection-state");
                let _ = events.send(PlayerEvent::ConnectionState(s));
            });
        }
        webrtc.connect_notify(Some("ice-connection-state"), |w, _| {
            let s = w.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
            tracing::info!("ICE connection state: {s:?}");
        });

        {
            let pipeline_weak = pipeline.downgrade();
            let events = events.clone();
            let stats = stats.clone();
            let config = config.clone();
            webrtc.connect_pad_added(move |_, pad| {
                let Some(pipeline) = pipeline_weak.upgrade() else { return };
                if pad.direction() != gst::PadDirection::Src {
                    return;
                }
                if let Err(e) = on_incoming_stream(&pipeline, pad, &config, &events, &stats) {
                    let _ = events.send(PlayerEvent::Error(format!("linking incoming stream: {e:#}")));
                }
            });
        }

        // --- bus ------------------------------------------------------------
        let bus = pipeline.bus().context("pipeline without bus")?;
        {
            let events = events.clone();
            let mut stream = bus.stream();
            tokio::spawn(async move {
                while let Some(msg) = stream.next().await {
                    use gst::MessageView;
                    match msg.view() {
                        MessageView::Error(err) => {
                            let text = format!(
                                "{} ({})",
                                err.error(),
                                err.debug().map(|d| d.to_string()).unwrap_or_default()
                            );
                            let closed = text.contains("window was closed") || text.contains("Window closed");
                            let _ = events.send(if closed { PlayerEvent::Closed } else { PlayerEvent::Error(text) });
                        }
                        MessageView::Eos(_) => {
                            let _ = events.send(PlayerEvent::Closed);
                        }
                        MessageView::Warning(w) => tracing::warn!("gstreamer: {}", w.error()),
                        _ => {}
                    }
                }
            });
        }

        pipeline.set_state(gst::State::Playing).context("starting pipeline")?;

        // --- data channels ---------------------------------------------------
        for kind in ChannelKind::ALL {
            let opts = gst::Structure::builder("config")
                .field("ordered", true)
                .field("protocol", kind.protocol())
                .build();
            let dc = webrtc
                .emit_by_name::<Option<WebRTCDataChannel>>("create-data-channel", &[&kind.label(), &opts])
                .ok_or_else(|| anyhow!("failed to create data channel {}", kind.label()))?;
            hook_data_channel(&dc, kind, events.clone());
            channels.lock().unwrap().insert(kind, dc);
        }

        Ok(Self { pipeline, webrtc, config, channels, candidates, gathering, mid_map: Mutex::new(Vec::new()), stats })
    }

    /// Creates the SDP offer, applies Greenlight's tweaks and sets it as local
    /// description. Returns the SDP text to send to the server.
    pub async fn create_offer(&self) -> Result<String> {
        let (p, rx) = promise();
        self.webrtc.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &p]);
        let reply = rx.await??.ok_or_else(|| anyhow!("empty create-offer reply"))?;
        let offer = reply
            .get::<WebRTCSessionDescription>("offer")
            .map_err(|_| anyhow!("create-offer failed: {reply}"))?;
        let text = offer.sdp().as_text()?;
        let munged = sdp::munge_offer(&text, self.config.stereo, self.config.video_kbps, self.config.audio_kbps);
        // Browser-style ICE credentials (4-char ufrag), like the Xbox web client.
        let (ufrag, pwd) = sdp::random_ice_credentials();
        let munged = sdp::set_ice_credentials(&munged, &ufrag, &pwd);
        let msg = gst_sdp::SDPMessage::parse_buffer(munged.as_bytes()).context("re-parsing munged SDP")?;
        let desc = WebRTCSessionDescription::new(WebRTCSDPType::Offer, msg);

        let (p, rx) = promise();
        self.webrtc.emit_by_name::<()>("set-local-description", &[&desc, &p]);
        rx.await??;
        // webrtcbin keeps its own mids (GStreamer >= 1.26 rejects renamed ones);
        // the console sees browser-style mids (0, 1, 2).
        let map = sdp::browser_mid_map(&munged);
        let wire = sdp::rename_mids(&munged, &map);
        *self.mid_map.lock().unwrap() = map;
        Ok(wire)
    }

    pub async fn set_remote_answer(&self, sdp: &str) -> Result<()> {
        let back: Vec<(String, String)> =
            self.mid_map.lock().unwrap().iter().map(|(native, wire)| (wire.clone(), native.clone())).collect();
        let sdp = sdp::rename_mids(sdp, &back);
        let msg = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()).context("parsing remote SDP")?;
        let desc = WebRTCSessionDescription::new(WebRTCSDPType::Answer, msg);
        let (p, rx) = promise();
        self.webrtc.emit_by_name::<()>("set-remote-description", &[&desc, &p]);
        if let Some(reply) = rx.await?? {
            if reply.has_field("error") {
                bail!("set-remote-description failed: {reply}");
            }
        }
        Ok(())
    }

    /// Waits for ICE gathering to complete (or `timeout`) and returns the local candidates.
    pub async fn local_candidates(&self, timeout: Duration) -> Vec<IceCandidate> {
        let mut rx = self.gathering.clone();
        let _ = tokio::time::timeout(timeout, rx.wait_for(|s| *s == WebRTCICEGatheringState::Complete)).await;
        // Browser-style mid for each candidate: the m-line index ("0", "1", ...),
        // matching the mids in the offer the console received.
        let map = self.mid_map.lock().unwrap().clone();
        let mut candidates = self.candidates.lock().unwrap().clone();
        for c in &mut candidates {
            if let Some((_, wire)) = c.sdp_m_line_index.and_then(|i| map.get(i as usize)) {
                c.sdp_mid = Some(wire.clone());
            }
        }
        candidates
    }

    pub fn add_remote_candidates(&self, candidates: &[IceCandidate]) {
        for c in sdp::expand_remote_candidates(candidates) {
            let mline = c.sdp_m_line_index.unwrap_or(0);
            tracing::debug!("remote candidate [{mline}]: {}", c.bare_candidate());
            self.webrtc.emit_by_name::<()>("add-ice-candidate", &[&mline, &c.bare_candidate()]);
        }
    }

    pub fn send(&self, out: &Outgoing) {
        let channels = self.channels.lock().unwrap();
        let Some(dc) = channels.get(&out.channel) else { return };
        if dc.ready_state() != gst_webrtc::WebRTCDataChannelState::Open {
            tracing::debug!("dropping message on {} (not open)", out.channel.label());
            return;
        }
        let bytes = glib::Bytes::from(&out.data[..]);
        dc.send_data(Some(&bytes));
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }

    pub fn webrtcbin(&self) -> &gst::Element {
        &self.webrtc
    }

    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Retransmission schedule for ICE checks: the first retry after `initial_ms`,
/// doubling each time, until about `secs` have passed. Returns
/// `(initial_ms, retransmissions)`.
pub fn ice_check_schedule(secs: u32) -> (u32, u32) {
    let initial_ms = 200u32;
    let budget_ms = secs.max(1) as u64 * 1000;
    let (mut total, mut delay, mut n) = (0u64, initial_ms as u64, 0u32);
    while total < budget_ms && n < 16 {
        total += delay;
        delay *= 2;
        n += 1;
    }
    (initial_ms, n.max(3))
}

/// Applies the ICE check window to webrtcbin's libnice agent.
fn configure_ice_timing(webrtc: &gst::Element, secs: u32) {
    let ice = webrtc.property::<glib::Object>("ice-agent");
    if !ice.has_property("agent", None) {
        tracing::warn!("ICE agent {} has no libnice agent; using default timing", ice.type_());
        return;
    }
    let agent = ice.property::<glib::Object>("agent");
    let (initial_ms, retransmissions) = ice_check_schedule(secs);
    agent.set_property("stun-initial-timeout", initial_ms);
    agent.set_property("stun-max-retransmissions", retransmissions);
    // Wait this long after all pairs failed before declaring ICE failed.
    agent.set_property("idle-timeout", 10_000u32);
    tracing::debug!(
        "ICE checks: first retry after {initial_ms} ms, {retransmissions} retransmissions (~{secs} s)"
    );
}

fn hook_data_channel(dc: &WebRTCDataChannel, kind: ChannelKind, events: mpsc::UnboundedSender<PlayerEvent>) {
    {
        let events = events.clone();
        dc.connect_on_open(move |_| {
            tracing::info!("data channel `{}` open", kind.label());
            let _ = events.send(PlayerEvent::ChannelOpen(kind));
        });
    }
    {
        let events = events.clone();
        dc.connect_on_close(move |_| {
            let _ = events.send(PlayerEvent::ChannelClosed(kind));
        });
    }
    {
        let events = events.clone();
        dc.connect_on_message_data(move |_, data| {
            if let Some(data) = data {
                let _ = events.send(PlayerEvent::ChannelMessage(kind, data.to_vec()));
            }
        });
    }
    {
        let events = events.clone();
        dc.connect_on_message_string(move |_, s| {
            if let Some(s) = s {
                let _ = events.send(PlayerEvent::ChannelMessage(kind, s.as_bytes().to_vec()));
            }
        });
    }
    dc.connect_on_error(move |_, err| {
        tracing::warn!("data channel `{}` error: {err}", kind.label());
    });
}

fn make_sink(desc: Option<&str>, default: &str) -> Result<gst::Element> {
    match desc {
        Some(d) => Ok(gst::parse::bin_from_description(d, true)?.upcast()),
        None => Ok(gst::ElementFactory::make(default).build()?),
    }
}

fn on_incoming_stream(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    config: &PlayerConfig,
    events: &mpsc::UnboundedSender<PlayerEvent>,
    stats: &Arc<Stats>,
) -> Result<()> {
    let caps = pad.current_caps().or_else(|| Some(pad.query_caps(None))).unwrap();
    let media = caps
        .structure(0)
        .and_then(|s| s.get::<&str>("media").ok())
        .unwrap_or("")
        .to_string();
    tracing::info!("incoming {media} stream: {caps}");

    // RTP probe: frame metadata and stats.
    {
        let events = events.clone();
        let stats = stats.clone();
        let is_video = media == "video";
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                if is_video {
                    stats.video_bytes.fetch_add(buffer.size() as u64, Ordering::Relaxed);
                    if let Ok(rtp) = gst_rtp::RTPBuffer::from_buffer_readable(buffer) {
                        if rtp.is_marker() {
                            stats.video_frames.fetch_add(1, Ordering::Relaxed);
                            let _ = events.send(PlayerEvent::VideoFrame {
                                rtp_timestamp: rtp.timestamp(),
                                arrival_ms: crate::input::packet::now_ms(),
                            });
                        }
                    }
                } else {
                    stats.audio_bytes.fetch_add(buffer.size() as u64, Ordering::Relaxed);
                }
            }
            gst::PadProbeReturn::Ok
        });
    }

    if media == "audio" && !config.audio {
        let fakesink = gst::ElementFactory::make("fakesink").property("async", false).build()?;
        pipeline.add(&fakesink)?;
        fakesink.sync_state_with_parent()?;
        pad.link(&fakesink.static_pad("sink").unwrap())?;
        return Ok(());
    }

    let decodebin = gst::ElementFactory::make("decodebin").build()?;
    {
        let pipeline_weak = pipeline.downgrade();
        let config = config.clone();
        let events = events.clone();
        decodebin.connect_pad_added(move |_, dpad| {
            let Some(pipeline) = pipeline_weak.upgrade() else { return };
            if let Err(e) = on_decoded_pad(&pipeline, dpad, &config, &events) {
                let _ = events.send(PlayerEvent::Error(format!("linking decoded stream: {e:#}")));
            }
        });
    }
    pipeline.add(&decodebin)?;
    decodebin.sync_state_with_parent()?;
    pad.link(&decodebin.static_pad("sink").unwrap())?;
    Ok(())
}

fn on_decoded_pad(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    config: &PlayerConfig,
    events: &mpsc::UnboundedSender<PlayerEvent>,
) -> Result<()> {
    let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
    let name = caps.structure(0).map(|s| s.name().to_string()).unwrap_or_default();

    let queue = gst::ElementFactory::make("queue")
        .property("max-size-buffers", 3u32)
        .property("max-size-time", 0u64)
        .property("max-size-bytes", 0u32)
        .build()?;

    let chain: Vec<gst::Element> = if name.starts_with("video/") {
        let convert = gst::ElementFactory::make("videoconvert").build()?;
        let sink = make_sink(config.video_sink.as_deref(), "autovideosink")?;
        if config.low_latency && sink.has_property("sync", None) {
            sink.set_property("sync", false);
        }
        // Keyboard input from the video window arrives as upstream navigation events.
        let events = events.clone();
        convert.static_pad("src").unwrap().add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
            if let Some(ev) = info.event() {
                if ev.type_() == gst::EventType::Navigation {
                    if let Ok(nav) = gst_video::NavigationEvent::parse(ev) {
                        match nav {
                            gst_video::NavigationEvent::KeyPress { key, .. } => {
                                let _ = events.send(PlayerEvent::KeyPress(key.to_string()));
                            }
                            gst_video::NavigationEvent::KeyRelease { key, .. } => {
                                let _ = events.send(PlayerEvent::KeyRelease(key.to_string()));
                            }
                            _ => {}
                        }
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
        vec![queue, convert, sink]
    } else if name.starts_with("audio/") {
        let convert = gst::ElementFactory::make("audioconvert").build()?;
        let resample = gst::ElementFactory::make("audioresample").build()?;
        let sink = make_sink(config.audio_sink.as_deref(), "autoaudiosink")?;
        if config.low_latency && sink.has_property("sync", None) {
            sink.set_property("sync", false);
        }
        vec![queue, convert, resample, sink]
    } else {
        tracing::warn!("ignoring decoded pad with caps {caps}");
        return Ok(());
    };

    pipeline.add_many(&chain)?;
    gst::Element::link_many(&chain)?;
    for e in &chain {
        e.sync_state_with_parent()?;
    }
    pad.link(&chain[0].static_pad("sink").unwrap())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ice_check_schedule;

    #[test]
    fn ice_schedule_covers_requested_window() {
        // 200 ms doubling: 200+400+...+12800 = 25.4 s, +25.6 s exceeds 30 s
        assert_eq!(ice_check_schedule(30), (200, 8));
        assert_eq!(ice_check_schedule(2), (200, 4));
        assert_eq!(ice_check_schedule(0).1, 3);
        assert!(ice_check_schedule(10_000).1 <= 16);
    }
}
