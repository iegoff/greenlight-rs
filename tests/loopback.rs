//! End-to-end WebRTC test: the greenlight [`Player`] negotiates with a fake
//! "console" built from a second `webrtcbin` that streams H.264 + Opus and
//! speaks the server side of the message/input channel handshake.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gst::prelude::*;
use gst_webrtc::{WebRTCDataChannel, WebRTCSDPType, WebRTCSessionDescription};
use tokio::sync::mpsc;

use greenlight::channels::{ChannelKind, ClientConfig, Protocol, ProtocolEvent};
use greenlight::input::packet::{report_type, GamepadFrame};
use greenlight::player::{Player, PlayerConfig, PlayerEvent};
use greenlight::sdp::IceCandidate;

struct FakeConsole {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    candidates: Arc<Mutex<Vec<IceCandidate>>>,
    /// (label, protocol, payload) of everything the client sent.
    received: Arc<Mutex<Vec<(String, String, Vec<u8>)>>>,
    channels: Arc<Mutex<Vec<WebRTCDataChannel>>>,
}

impl FakeConsole {
    fn new() -> Self {
        // ultrafast x264 produces constrained-baseline, which the client offers as pt 103;
        // capssetter advertises the offered level so strict matchers (GStreamer >= 1.26) accept it.
        let pipeline = gst::parse::launch(
            "webrtcbin name=srv bundle-policy=max-bundle \
             audiotestsrc is-live=true wave=sine ! audioconvert ! audioresample ! opusenc ! \
               rtpopuspay pt=111 ! application/x-rtp,media=audio,encoding-name=OPUS,payload=111 ! queue ! srv. \
             videotestsrc is-live=true pattern=ball ! video/x-raw,format=I420,width=320,height=240,framerate=30/1 ! \
               x264enc tune=zerolatency speed-preset=ultrafast key-int-max=30 ! \
               rtph264pay config-interval=-1 pt=103 ! \
               capssetter caps=\"application/x-rtp,profile-level-id=(string)42e01f\" ! \
               application/x-rtp,media=video,encoding-name=H264,payload=103 ! queue ! srv.",
        )
        .expect("fake console pipeline")
        .downcast::<gst::Pipeline>()
        .unwrap();
        let webrtc = pipeline.by_name("srv").unwrap();

        // Minimal codec preferences so the answerer matches the offered m-lines
        // regardless of encoder-specific fields (profile-level-id, sprop-stereo).
        // GStreamer >= 1.26 intersects these strictly at set-remote-description.
        // Keyed by pad name: the transceiver kind is unknown until caps flow.
        // extmap-3 = transport-wide CC, which the real console also answers with.
        for (pad, caps) in [
            ("sink_0", "application/x-rtp,media=audio,encoding-name=OPUS,clock-rate=48000,payload=111,extmap-3=(string)http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"),
            ("sink_1", "application/x-rtp,media=video,encoding-name=H264,clock-rate=90000,payload=103,extmap-3=(string)http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"),
        ] {
            let tr = webrtc
                .static_pad(pad)
                .unwrap()
                .property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");
            tr.set_property("codec-preferences", caps.parse::<gst::Caps>().unwrap());
        }


        let candidates: Arc<Mutex<Vec<IceCandidate>>> = Default::default();
        {
            let candidates = candidates.clone();
            webrtc.connect("on-ice-candidate", false, move |v| {
                let mline = v[1].get::<u32>().unwrap();
                let c = v[2].get::<String>().unwrap();
                candidates.lock().unwrap().push(IceCandidate {
                    candidate: format!("a={c}"),
                    sdp_m_line_index: Some(mline),
                    sdp_mid: Some(mline.to_string()),
                    message_type: Some("iceCandidate".into()),
                    username_fragment: None,
                });
                None
            });
        }

        let received: Arc<Mutex<Vec<(String, String, Vec<u8>)>>> = Default::default();
        let channels: Arc<Mutex<Vec<WebRTCDataChannel>>> = Default::default();
        {
            let received = received.clone();
            let channels = channels.clone();
            webrtc.connect("on-data-channel", false, move |v| {
                let dc = v[1].get::<WebRTCDataChannel>().unwrap();
                let label = dc.label().map(|s| s.to_string()).unwrap_or_default();
                let protocol = dc.protocol().map(|s| s.to_string()).unwrap_or_default();
                {
                    let received = received.clone();
                    let (label, protocol) = (label.clone(), protocol.clone());
                    dc.connect_on_message_data(move |dc, data| {
                        let data = data.map(|d| d.to_vec()).unwrap_or_default();
                        if label == "message" {
                            let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
                            if v["type"] == "Handshake" {
                                dc.send_data(Some(&glib::Bytes::from_static(
                                    br#"{"type":"HandshakeAck","version":"messageV1","id":"x","cv":"0"}"#,
                                )));
                            }
                        }
                        received.lock().unwrap().push((label.clone(), protocol.clone(), data));
                    });
                }
                channels.lock().unwrap().push(dc);
                None
            });
        }

        pipeline.set_state(gst::State::Playing).unwrap();
        Self { pipeline, webrtc, candidates, received, channels }
    }

    async fn answer(&self, offer_sdp: &str) -> String {
        // Like a real console, only answer once our encoders produce caps.
        for _ in 0..100 {
            if self.webrtc.sink_pads().iter().all(|p| p.current_caps().is_some()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if std::env::var("LOOPBACK_DEBUG").is_ok() {
            for p in self.webrtc.sink_pads() {
                eprintln!("FAKE {} caps: {:?}", p.name(), p.current_caps());
            }
        }
        let msg = gst_sdp::SDPMessage::parse_buffer(offer_sdp.as_bytes()).unwrap();
        let offer = WebRTCSessionDescription::new(WebRTCSDPType::Offer, msg);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let p = gst::Promise::with_change_func(move |_| {
            let _ = tx.send(());
        });
        self.webrtc.emit_by_name::<()>("set-remote-description", &[&offer, &p]);
        rx.await.unwrap();


        let (tx, rx) = tokio::sync::oneshot::channel();
        let p = gst::Promise::with_change_func(move |r| {
            let s = r.unwrap().unwrap();
            let _ = tx.send(s.get::<WebRTCSessionDescription>("answer").unwrap());
        });
        self.webrtc.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &p]);
        let answer = rx.await.unwrap();
        self.webrtc
            .emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
        answer.sdp().as_text().unwrap()
    }

    fn add_candidates(&self, c: &[IceCandidate]) {
        for c in c {
            self.webrtc.emit_by_name::<()>(
                "add-ice-candidate",
                &[&c.sdp_m_line_index.unwrap_or(0), &c.bare_candidate()],
            );
        }
    }
}

impl Drop for FakeConsole {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn negotiates_streams_and_channels_with_fake_console() {
    gst::init().unwrap();
    if std::env::var("LOOPBACK_DEBUG").is_ok() {
        let _ = tracing_subscriber::fmt().with_env_filter("greenlight=debug").try_init();
    }
    let console = FakeConsole::new();

    let (tx, mut events) = mpsc::unbounded_channel();
    let config = PlayerConfig {
        // Prebuilt sink element, like the GUI's gtk4paintablesink.
        video_sink_element: Some(gst::ElementFactory::make("fakesink").property("sync", false).build().unwrap()),
        audio_sink: Some("fakesink sync=false".into()),
        ..Default::default()
    };
    let player = Player::new(config, tx).unwrap();

    // By default libnice's short check window is kept (see PlayerConfig::ice_check_secs).
    let agent = player
        .webrtcbin()
        .property::<glib::Object>("ice-agent")
        .property::<glib::Object>("agent");
    assert_eq!(agent.property::<u32>("stun-max-retransmissions"), 3);

    let offer = player.create_offer().await.unwrap();
    assert!(offer.contains("m=audio"), "{offer}");
    assert!(offer.contains("m=video"), "{offer}");
    assert!(offer.contains("m=application"), "{offer}");
    assert!(offer.contains("stereo=1"), "{offer}");
    assert!(offer.contains("profile-level-id=4d001f"), "{offer}");
    // Transport-wide congestion control header extension, as in the web client's offer.
    assert_eq!(
        offer.matches("a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01").count(),
        2,
        "TWCC extmap missing on audio/video: {offer}"
    );
    // Browser-style offer: numeric mids and a 4-character ICE ufrag.
    assert!(offer.contains("a=group:BUNDLE 0 1 2"), "{offer}");
    let ufrag = offer.lines().find_map(|l| l.strip_prefix("a=ice-ufrag:")).unwrap().trim();
    assert_eq!(ufrag.len(), 4, "{offer}");

    let answer = console.answer(&offer).await;
    player.set_remote_answer(&answer).await.unwrap();

    let local = player.local_candidates(Duration::from_secs(5)).await;
    assert!(!local.is_empty(), "no local candidates");
    assert!(local.iter().all(|c| c.sdp_mid.as_deref() == Some("0")), "candidates must carry the SDP mid: {local:?}");
    // Round-trip through the JSON wire format used by the xCloud API.
    let json = serde_json::to_string(&local).unwrap();
    let local: Vec<IceCandidate> = serde_json::from_str(&json).unwrap();
    console.add_candidates(&local);

    tokio::time::sleep(Duration::from_millis(500)).await;
    let remote = console.candidates.lock().unwrap().clone();
    assert!(!remote.is_empty(), "fake console produced no candidates");
    player.add_remote_candidates(&remote);

    let mut protocol = Protocol::new(ClientConfig::default());
    let mut opened = std::collections::HashSet::new();
    let mut handshake = false;
    let mut video_frames = 0;
    let mut audio_or_video_connected = false;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while tokio::time::Instant::now() < deadline {
        let ev = match tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => break,
            Err(_) => {
                if let Some(out) = protocol.tick(GamepadFrame::new(0), greenlight::input::packet::now_ms(), 33.0) {
                    player.send(&out);
                }
                continue;
            }
        };
        match ev {
            PlayerEvent::ChannelOpen(k) => {
                opened.insert(k);
                for o in protocol.on_open(k) {
                    player.send(&o);
                }
            }
            PlayerEvent::ChannelMessage(k, data) => {
                let (out, evs) = protocol.on_message(k, &data);
                for o in &out {
                    player.send(o);
                }
                if evs.contains(&ProtocolEvent::HandshakeCompleted) {
                    handshake = true;
                }
            }
            PlayerEvent::ConnectionState(s) => {
                if s == gst_webrtc::WebRTCPeerConnectionState::Connected {
                    audio_or_video_connected = true;
                }
            }
            PlayerEvent::VideoFrame { .. } => video_frames += 1,
            PlayerEvent::Error(e) => panic!("player error: {e}"),
            _ => {}
        }
        if handshake && video_frames > 10 && opened.len() == 4 {
            // Let a few input packets flow.
            for _ in 0..5 {
                if let Some(out) = protocol.tick(GamepadFrame::new(0), greenlight::input::packet::now_ms() + 100.0, 33.0) {
                    player.send(&out);
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            break;
        }
    }

    assert!(audio_or_video_connected, "peer connection never reached Connected");
    assert_eq!(opened.len(), 4, "not all data channels opened: {opened:?}");
    assert!(handshake, "message channel handshake not completed");
    assert!(video_frames > 10, "only {video_frames} video frames received");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let received = console.received.lock().unwrap().clone();
    let protocols: std::collections::HashMap<_, _> =
        console.channels.lock().unwrap().iter().map(|d| (d.label().unwrap().to_string(), d.protocol().unwrap().to_string())).collect();
    for k in ChannelKind::ALL {
        assert_eq!(protocols.get(k.label()).map(String::as_str), Some(k.protocol()), "{protocols:?}");
    }

    let control: Vec<serde_json::Value> = received
        .iter()
        .filter(|(l, _, _)| l == "control")
        .map(|(_, _, d)| serde_json::from_slice(d).unwrap())
        .collect();
    assert_eq!(control[0]["message"], "authorizationRequest");

    let input: Vec<&Vec<u8>> = received.iter().filter(|(l, _, _)| l == "input").map(|(_, _, d)| d).collect();
    assert!(!input.is_empty(), "no input packets");
    assert_eq!(input[0].len(), 15);
    assert_eq!(u16::from_le_bytes([input[0][0], input[0][1]]), report_type::CLIENT_METADATA);
    // Later packets carry gamepad state and/or video frame metadata.
    assert!(input.iter().skip(1).any(|p| {
        let t = u16::from_le_bytes([p[0], p[1]]);
        t & (report_type::GAMEPAD | report_type::METADATA) != 0
    }));

    let messages: Vec<serde_json::Value> = received
        .iter()
        .filter(|(l, _, _)| l == "message")
        .map(|(_, _, d)| serde_json::from_slice(d).unwrap())
        .collect();
    assert!(messages
        .iter()
        .any(|m| m["target"] == "/streaming/characteristics/dimensionschanged"));

    // Decoded audio and video must actually reach the sinks.
    let mut sink_caps = Vec::new();
    for el in player.pipeline().iterate_recurse().into_iter().flatten() {
        if el.factory().map(|f| f.name() == "fakesink").unwrap_or(false) {
            if let Some(sample) = el.property::<Option<gst::Sample>>("last-sample") {
                sink_caps.push(sample.caps().unwrap().structure(0).unwrap().name().to_string());
            }
        }
    }
    assert!(sink_caps.iter().any(|c| c.starts_with("video/x-raw")), "no decoded video: {sink_caps:?}");
    assert!(sink_caps.iter().any(|c| c.starts_with("audio/x-raw")), "no decoded audio: {sink_caps:?}");

    // Loss handling: keyframe wait/request on video, FEC + PLC on audio.
    let mut tuned = (false, false);
    for el in player.pipeline().iterate_recurse().into_iter().flatten() {
        match el.factory().map(|f| f.name().to_string()).as_deref() {
            Some("rtph264depay") => {
                assert!(el.property::<bool>("wait-for-keyframe"));
                assert!(el.property::<bool>("request-keyframe"));
                tuned.0 = true;
            }
            Some("opusdec") => {
                assert!(el.property::<bool>("use-inband-fec"));
                tuned.1 = true;
            }
            _ => {}
        }
    }
    assert_eq!(tuned, (true, true), "depayloader/decoder not found or not tuned");

    // The fake console (the sender) must receive TWCC feedback from us.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let p = gst::Promise::with_change_func(move |r| {
        let _ = tx.send(r.ok().flatten().map(|s| s.to_string()));
    });
    console.webrtc.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &p]);
    let console_stats = rx.await.unwrap().unwrap_or_default();
    if std::env::var("LOOPBACK_DEBUG").is_ok() {
        for part in console_stats.split("twcc").skip(1).take(2) {
            eprintln!("TWCC: twcc{}", &part[..part.len().min(400)]);
        }
    }
    // twcc-stats is escaped inside the stats string; pull out packets-recv.
    let acked: u64 = console_stats
        .split("twcc-stats")
        .nth(1)
        .and_then(|t| t.split("packets-recv").nth(1))
        .and_then(|t| t.split("uint").nth(1))
        .map(|t| t.chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit()).collect::<String>())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(acked > 0, "the sender received no TWCC feedback from the client: {}", &console_stats[..console_stats.len().min(300)]);

    // Network stats + decoder name for the overlay.
    let stats = player.webrtc_stats().await;
    if std::env::var("LOOPBACK_DEBUG").is_ok() {
        eprintln!("STATS: {}", greenlight::session::format_stats(&stats));
    }
    assert!(stats.decoder.is_some(), "no decoder found: {stats:?}");
    assert!(stats.packets_lost.is_some(), "no inbound-rtp packets-lost: {stats:?}");

    player.stop();
}
