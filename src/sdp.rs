//! SDP and ICE helpers shared by the player and the signalling layer.

use serde::{Deserialize, Serialize};

use crate::teredo;

/// Applies the same tweaks Greenlight makes to the local offer.
pub fn munge_offer(sdp: &str, stereo: bool, video_kbps: u32, audio_kbps: u32) -> String {
    let mut sdp = sdp.to_string();
    if stereo && !sdp.contains("stereo=1") {
        sdp = sdp.replace("useinbandfec=1", "useinbandfec=1; stereo=1");
    }
    if video_kbps > 0 {
        sdp = set_bitrate(&sdp, "video", video_kbps);
    }
    if audio_kbps > 0 {
        sdp = set_bitrate(&sdp, "audio", audio_kbps);
    }
    sdp
}

/// Inserts (or replaces) a `b=AS:` line for the first media section of `media`.
pub fn set_bitrate(sdp: &str, media: &str, kbps: u32) -> String {
    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = sdp.lines().map(str::to_string).collect();
    let Some(mut idx) = lines.iter().position(|l| l.starts_with(&format!("m={media}"))) else {
        return sdp.to_string();
    };
    idx += 1;
    while idx < lines.len() && (lines[idx].starts_with("i=") || lines[idx].starts_with("c=")) {
        idx += 1;
    }
    let bline = format!("b=AS:{kbps}");
    if idx < lines.len() && lines[idx].starts_with('b') {
        lines[idx] = bline;
    } else {
        lines.insert(idx, bline);
    }
    let mut out = lines.join(eol);
    if sdp.ends_with('\n') {
        out.push_str(eol);
    }
    out
}

/// Returns the `a=mid:` value of every media section, in m-line order.
/// Sections without a mid get their index as a string.
pub fn media_mids(sdp: &str) -> Vec<String> {
    let mut mids: Vec<String> = Vec::new();
    for line in sdp.lines() {
        if line.starts_with("m=") {
            mids.push((mids.len()).to_string());
        } else if let (Some(mid), Some(last)) = (line.strip_prefix("a=mid:"), mids.last_mut()) {
            *last = mid.trim().to_string();
        }
    }
    mids
}

/// An ICE candidate in the JSON shape used by the xCloud signalling API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(default, deserialize_with = "de_string_or_number")]
    pub sdp_m_line_index: Option<u32>,
    #[serde(default)]
    pub sdp_mid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_type: Option<String>,
}

fn de_string_or_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        Some(serde_json::Value::Number(n)) => n.as_u64().map(|n| n as u32),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        _ => None,
    })
}

impl IceCandidate {
    pub fn is_end_of_candidates(&self) -> bool {
        let c = self.candidate.trim();
        c.is_empty() || c == "a=end-of-candidates" || c == "end-of-candidates"
    }

    /// Candidate line without the `a=` prefix, as expected by webrtcbin.
    pub fn bare_candidate(&self) -> &str {
        let c = self.candidate.trim();
        c.strip_prefix("a=").unwrap_or(c)
    }

    /// Address field of the candidate (`candidate:<f> <c> <proto> <prio> <addr> <port> ...`).
    pub fn address(&self) -> Option<&str> {
        self.bare_candidate().split_whitespace().nth(4)
    }
}

/// Expands remote candidates: every Teredo candidate gets two additional IPv4
/// host candidates (port 9002 and the embedded Teredo port), as Greenlight does.
/// End-of-candidate markers are dropped.
pub fn expand_remote_candidates(candidates: &[IceCandidate]) -> Vec<IceCandidate> {
    let mut out = Vec::new();
    for c in candidates {
        if c.is_end_of_candidates() {
            continue;
        }
        if let Some(info) = c.address().filter(|a| a.starts_with("2001")).and_then(teredo::parse) {
            for (foundation, port) in [(10, 9002u16), (11, info.udp_port)] {
                out.push(IceCandidate {
                    candidate: format!(
                        "candidate:{foundation} 1 UDP 1 {} {port} typ host",
                        info.client
                    ),
                    sdp_m_line_index: c.sdp_m_line_index,
                    sdp_mid: c.sdp_mid.clone(),
                    message_type: None,
                });
            }
        }
        out.push(c.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=fmtp:111 minptime=10;useinbandfec=1\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:96 H264/90000\r\n";

    #[test]
    fn stereo_and_bitrate() {
        let s = munge_offer(OFFER, true, 12000, 0);
        assert!(s.contains("useinbandfec=1; stereo=1"));
        assert!(s.contains("m=video 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\nb=AS:12000\r\n"));
        assert!(s.ends_with("\r\n"));
        // idempotent replacement
        let s2 = set_bitrate(&s, "video", 5000);
        assert!(s2.contains("b=AS:5000") && !s2.contains("b=AS:12000"));
    }

    #[test]
    fn mids() {
        let sdp = "v=0\r\na=group:BUNDLE audio0 video1\r\nm=audio 9 X 111\r\na=mid:audio0\r\nm=video 9 X 96\r\na=mid:video1\r\nm=application 9 X webrtc-datachannel\r\n";
        assert_eq!(media_mids(sdp), vec!["audio0", "video1", "2"]);
    }

    #[test]
    fn teredo_expansion() {
        let json = r#"[
            {"candidate":"a=candidate:1 1 UDP 100 2001:0000:4136:e378:8000:63bf:3fff:fdd2 1234 typ host ","messageType":"iceCandidate","sdpMLineIndex":"0","sdpMid":"0"},
            {"candidate":"a=candidate:2 1 UDP 90 10.0.0.2 9002 typ host ","sdpMLineIndex":0,"sdpMid":"0"},
            {"candidate":"a=end-of-candidates","sdpMLineIndex":"0","sdpMid":"0"}
        ]"#;
        let c: Vec<IceCandidate> = serde_json::from_str(json).unwrap();
        let out = expand_remote_candidates(&c);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].candidate, "candidate:10 1 UDP 1 192.0.2.45 9002 typ host");
        assert_eq!(out[1].candidate, "candidate:11 1 UDP 1 192.0.2.45 40000 typ host");
        assert_eq!(out[3].bare_candidate(), "candidate:2 1 UDP 90 10.0.0.2 9002 typ host");
        assert_eq!(out[3].sdp_m_line_index, Some(0));
    }
}
