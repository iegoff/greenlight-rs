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

fn rewrite_lines(sdp: &str, f: impl Fn(&str) -> String) -> String {
    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut s = sdp.lines().map(|l| f(l)).collect::<Vec<_>>().join(eol);
    if sdp.ends_with('\n') {
        s.push_str(eol);
    }
    s
}

/// Replaces the ICE username fragment and password of every m-line.
pub fn set_ice_credentials(sdp: &str, ufrag: &str, pwd: &str) -> String {
    rewrite_lines(sdp, |line| {
        if line.starts_with("a=ice-ufrag:") {
            format!("a=ice-ufrag:{ufrag}")
        } else if line.starts_with("a=ice-pwd:") {
            format!("a=ice-pwd:{pwd}")
        } else {
            line.to_string()
        }
    })
}

/// Renames media IDs (`a=mid:` and the BUNDLE group) using `(from, to)` pairs.
/// Used to show the console browser-style mids (`0`, `1`, `2`) while
/// webrtcbin keeps its own (`audio0`, `video1`, ...), and back for the answer.
pub fn rename_mids(sdp: &str, map: &[(String, String)]) -> String {
    let lookup = |m: &str| {
        map.iter().find(|(from, _)| from == m).map(|(_, to)| to.clone()).unwrap_or_else(|| m.to_string())
    };
    rewrite_lines(sdp, |line| {
        if let Some(mid) = line.strip_prefix("a=mid:") {
            format!("a=mid:{}", lookup(mid.trim()))
        } else if let Some(group) = line.strip_prefix("a=group:BUNDLE") {
            let ids: Vec<String> = group.split_whitespace().map(lookup).collect();
            format!("a=group:BUNDLE {}", ids.join(" "))
        } else {
            line.to_string()
        }
    })
}

/// `(native mid, m-line index)` pairs, i.e. the mapping to browser-style mids.
pub fn browser_mid_map(sdp: &str) -> Vec<(String, String)> {
    media_mids(sdp).into_iter().enumerate().map(|(i, m)| (m, i.to_string())).collect()
}

/// Random ICE credentials in the browser style (4-char ufrag, 24-char pwd).
pub fn random_ice_credentials() -> (String, String) {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let more = *uuid::Uuid::new_v4().as_bytes();
    let pick = |b: u8| CHARS[b as usize % CHARS.len()] as char;
    let ufrag: String = bytes[..4].iter().map(|b| pick(*b)).collect();
    let pwd: String = bytes[4..16].iter().chain(more[..12].iter()).map(|b| pick(*b)).collect();
    (ufrag, pwd)
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
    /// ICE ufrag of the candidate's generation (browsers send it; the Xbox web
    /// client includes it when posting local candidates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username_fragment: Option<String>,
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
                    username_fragment: None,
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
    fn browser_style_offer() {
        let sdp = "v=0\r\na=group:BUNDLE audio0 video1 application2\r\nm=audio 9 X 111\r\na=ice-ufrag:LONGLONGLONG/+\r\na=ice-pwd:oldpwd\r\na=mid:audio0\r\nm=video 9 X 96\r\na=ice-ufrag:LONGLONGLONG/+\r\na=mid:video1\r\nm=application 9 X webrtc-datachannel\r\na=mid:application2\r\n";
        let local = set_ice_credentials(sdp, "AbC1", &"p".repeat(24));
        assert_eq!(local.matches("a=ice-ufrag:AbC1\r\n").count(), 2);
        assert!(local.contains("a=mid:audio0") && !local.contains("LONG"));

        let map = browser_mid_map(&local);
        let wire = rename_mids(&local, &map);
        assert!(wire.contains("a=group:BUNDLE 0 1 2\r\n"), "{wire}");
        assert!(wire.contains("a=mid:0\r\n") && wire.contains("a=mid:2\r\n"));
        assert!(!wire.contains("audio0") && wire.ends_with("\r\n"));

        // and back, for the answer
        let back: Vec<(String, String)> = map.iter().map(|(a, b)| (b.clone(), a.clone())).collect();
        assert_eq!(rename_mids(&wire, &back), local);

        let (u, p) = random_ice_credentials();
        assert_eq!((u.len(), p.len()), (4, 24));
        assert!(u.chars().chain(p.chars()).all(|c| c.is_ascii_alphanumeric()));
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
