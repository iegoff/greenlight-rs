//! GSSV streaming API (`*.gssv-play-prod.xboxlive.com`), used for both
//! xCloud (`cloud`) and Xbox home streaming (`home`).

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::http::Http;
use crate::sdp::IceCandidate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Home,
    Cloud,
}

impl StreamKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamKind::Home => "home",
            StreamKind::Cloud => "cloud",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayResponse {
    pub session_path: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

impl PlayResponse {
    /// `v5/sessions/home/<ID>` → `<ID>`
    pub fn id(&self) -> String {
        self.session_id
            .clone()
            .unwrap_or_else(|| self.session_path.rsplit('/').next().unwrap_or_default().to_string())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorDetails {
    #[serde(default)]
    pub code: Option<Value>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateResponse {
    pub state: String,
    #[serde(default)]
    pub error_details: Option<ErrorDetails>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeResponse {
    #[serde(default)]
    pub exchange_response: Option<String>,
    #[serde(default)]
    pub error_details: Option<ErrorDetails>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SdpAnswer {
    pub sdp: String,
    #[serde(default)]
    pub sdp_type: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Clone)]
pub struct XCloudApi {
    http: Http,
    base: String,
    token: String,
    kind: StreamKind,
    locale: String,
    display: (u32, u32),
}

impl XCloudApi {
    /// `base` is the region base URI, e.g. `https://uks.core.gssv-play-prod.xboxlive.com`.
    pub fn new(http: Http, base: &str, token: &str, kind: StreamKind, locale: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            kind,
            locale: locale.to_string(),
            display: (1920, 1080),
        }
    }

    /// Display size reported to the server in the device info.
    pub fn set_display(&mut self, width: u32, height: u32) {
        self.display = (width, height);
    }

    pub fn kind(&self) -> StreamKind {
        self.kind
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Content-Type", "application/json".into()),
            ("Authorization", format!("Bearer {}", self.token)),
            ("X-Gssv-Client", "XboxComBrowser".into()),
        ]
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    fn session(&self, id: &str, rest: &str) -> String {
        format!("v5/sessions/{}/{}{}", self.kind.as_str(), id, rest)
    }

    async fn get(&self, path: &str) -> Result<(StatusCode, Value)> {
        self.http.send(Method::GET, &self.url(path), &self.headers(), None).await
    }

    /// GET that retries while the server answers `204 No Content`.
    async fn get_until_ready(&self, path: &str, timeout: Duration) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (status, body) = self.get(path).await?;
            if status != StatusCode::NO_CONTENT {
                return Ok(body);
            }
            if tokio::time::Instant::now() > deadline {
                return Err(anyhow!("timed out waiting for {path}"));
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
    }

    async fn post(&self, path: &str, body: &Value, extra: &[(&'static str, String)]) -> Result<Value> {
        let mut headers = self.headers();
        headers.extend_from_slice(extra);
        let (status, resp) = self.http.send(Method::POST, &self.url(path), &headers, Some(body)).await?;
        tracing::debug!("POST {path} -> {status} {resp}");
        Ok(resp)
    }

    pub async fn titles(&self) -> Result<Value> {
        Ok(self.get("v2/titles").await?.1)
    }

    pub async fn recent_titles(&self) -> Result<Value> {
        Ok(self.get("v2/titles/mru?mr=25").await?.1)
    }

    pub async fn waiting_times(&self, title_id: &str) -> Result<Value> {
        Ok(self.get(&format!("v1/waittime/{title_id}")).await?.1)
    }

    pub async fn active_sessions(&self) -> Result<Value> {
        Ok(self.get(&format!("v5/sessions/{}/active", self.kind.as_str())).await?.1)
    }

    pub async fn start_stream(&self, target: &str) -> Result<PlayResponse> {
        let body = json!({
            "titleId": if self.kind == StreamKind::Cloud { target } else { "" },
            "systemUpdateGroup": "",
            "clientSessionId": client_session_id(),
            "settings": {
                "nanoVersion": "V3;WebrtcTransport.dll",
                "enableTextToSpeech": false,
                "enableOptionalDataCollection": false,
                "highContrast": 0,
                "locale": self.locale,
                "useIceConnection": false,
                "timezoneOffsetMinutes": timezone_offset_minutes(),
                "sdkType": "web",
                "osName": "linux",
            },
            "serverId": if self.kind == StreamKind::Home { target } else { "" },
            "fallbackRegionNames": [],
        });
        let resp = self
            .post(
                &format!("v5/sessions/{}/play", self.kind.as_str()),
                &body,
                &[("X-MS-Device-Info", device_info(self.display.0, self.display.1))],
            )
            .await
            .context("starting stream session")?;
        Ok(serde_json::from_value(resp)?)
    }

    /// Session configuration (console address, STUN servers, ...). The web
    /// client fetches this after provisioning, before the SDP exchange.
    pub async fn configuration(&self, session_id: &str) -> Result<Value> {
        Ok(self.get(&self.session(session_id, "/configuration")).await?.1)
    }

    pub async fn state(&self, session_id: &str) -> Result<StateResponse> {
        let (_, body) = self.get(&self.session(session_id, "/state")).await?;
        Ok(serde_json::from_value(body)?)
    }

    pub async fn stop_stream(&self, session_id: &str) -> Result<()> {
        self.http
            .send(Method::DELETE, &self.url(&self.session(session_id, "")), &self.headers(), None)
            .await?;
        Ok(())
    }

    pub async fn connect(&self, session_id: &str, user_token: &str) -> Result<()> {
        self.post(&self.session(session_id, "/connect"), &json!({ "userToken": user_token }), &[])
            .await?;
        Ok(())
    }

    pub async fn keepalive(&self, session_id: &str) -> Result<Value> {
        self.post(&self.session(session_id, "/keepalive"), &json!({}), &[]).await
    }

    /// Sends the local SDP offer and waits for the console/server answer.
    pub async fn exchange_sdp(&self, session_id: &str, sdp: &str) -> Result<SdpAnswer> {
        let body = json!({
            "messageType": "offer",
            "sdp": sdp,
            "requestId": 1,
            "configuration": {
                "chatConfiguration": {
                    "bytesPerSample": 2,
                    "expectedClipDurationMs": 20,
                    "format": { "codec": "opus", "container": "webm" },
                    "numChannels": 1,
                    "sampleFrequencyHz": 24000,
                },
                "chat": { "minVersion": 1, "maxVersion": 1 },
                "chatStream": { "minVersion": 1, "maxVersion": 1 },
                "control": { "minVersion": 1, "maxVersion": 3 },
                "input": { "minVersion": 1, "maxVersion": 9 },
                "message": { "minVersion": 1, "maxVersion": 1 },
                "reliableinput": { "minVersion": 9, "maxVersion": 9 },
                "unreliableinput": { "minVersion": 9, "maxVersion": 9 },
                "isDataChannelOnly": false,
            },
        });
        let path = self.session(session_id, "/sdp");
        self.post(&path, &body, &[]).await.context("posting SDP offer")?;
        let resp: ExchangeResponse =
            serde_json::from_value(self.get_until_ready(&path, Duration::from_secs(30)).await?)?;
        let exchange = resp.exchange_response.ok_or_else(|| {
            anyhow!(
                "SDP exchange failed: {}",
                resp.error_details.and_then(|e| e.message).unwrap_or_else(|| "no answer".into())
            )
        })?;
        let answer: SdpAnswer = serde_json::from_str(&exchange).context("parsing SDP answer")?;
        if answer.status.as_deref().is_some_and(|s| !s.eq_ignore_ascii_case("success")) {
            return Err(anyhow!("SDP answer status: {:?}", answer.status));
        }
        Ok(answer)
    }

    /// Sends local ICE candidates and returns the remote ones.
    pub async fn exchange_ice(&self, session_id: &str, candidates: &[IceCandidate]) -> Result<Vec<IceCandidate>> {
        let body = ice_post_body(candidates);
        let path = self.session(session_id, "/ice");
        self.post(&path, &body, &[]).await.context("posting ICE candidates")?;
        let resp: ExchangeResponse =
            serde_json::from_value(self.get_until_ready(&path, Duration::from_secs(30)).await?)?;
        let exchange = resp.exchange_response.ok_or_else(|| anyhow!("ICE exchange returned no candidates"))?;
        serde_json::from_str(&exchange).context("parsing remote ICE candidates")
    }
}

/// Body for `POST .../ice`, in the format the Xbox web client (xbox.com) uses:
/// `{"candidates": ["<candidate as JSON string>", ..., "a=end-of-candidates"]}`.
/// Each entry is a JSON-*encoded string* of `RTCIceCandidate.toJSON()`.
/// (The older `{"messageType": "iceCandidate", "candidate": [...]}` shape used by
/// Greenlight is accepted by the server but never reaches the console.)
pub fn ice_post_body(candidates: &[IceCandidate]) -> Value {
    let mut list: Vec<Value> = candidates
        .iter()
        .filter(|c| !c.is_end_of_candidates())
        .map(|c| {
            let mut o = serde_json::Map::new();
            o.insert("candidate".into(), json!(c.bare_candidate()));
            o.insert("sdpMid".into(), json!(c.sdp_mid.clone().unwrap_or_else(|| c.sdp_m_line_index.unwrap_or(0).to_string())));
            o.insert("sdpMLineIndex".into(), json!(c.sdp_m_line_index.unwrap_or(0)));
            if let Some(u) = &c.username_fragment {
                o.insert("usernameFragment".into(), json!(u));
            }
            Value::String(Value::Object(o).to_string())
        })
        .collect();
    list.push(json!("a=end-of-candidates"));
    json!({ "candidates": list })
}

fn timezone_offset_minutes() -> i32 {
    use chrono::Local;
    Local::now().offset().local_minus_utc() / 60
}

/// Device description sent with `/play`. Mirrors the xbox.com web client
/// (play.xbox.com in Edge on Linux), which is the signalling flavour this
/// client implements.
pub fn device_info(width: u32, height: u32) -> String {
    json!({
        "appInfo": {
            "env": {
                "clientAppId": "play.xbox.com",
                "clientAppType": "browser",
                "clientSdkVersion": "placeholder",
                "httpEnvironment": "prod",
                "sdkInstallId": "",
            }
        },
        "dev": {
            "displayInfo": {
                "dimensions": { "heightInPixels": height, "widthInPixels": width },
                "pixelDensity": { "dpiX": 1, "dpiY": 1 },
            },
            "browser": { "browserName": "edge", "browserVersion": "158.0.4370.45" },
            "hw": { "make": "unknown", "model": "unknown", "platformType": "desktop", "sdkType": "web" },
            "os": { "name": "linux", "ver": "unknown", "platform": "desktop" },
        },
    })
    .to_string()
}

/// 22-character random id, like the web client's `clientSessionId`.
fn client_session_id() -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let a = *uuid::Uuid::new_v4().as_bytes();
    let b = *uuid::Uuid::new_v4().as_bytes();
    a.iter().chain(b.iter()).take(22).map(|x| CHARS[*x as usize % CHARS.len()] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_from_path() {
        let p: PlayResponse = serde_json::from_value(json!({
            "sessionPath": "v5/sessions/home/ABCDEF-123",
            "state": "Provisioning"
        }))
        .unwrap();
        assert_eq!(p.id(), "ABCDEF-123");
    }

    #[test]
    fn ice_body_matches_web_client() {
        let c = IceCandidate {
            candidate: "candidate:1 1 UDP 2015363327 192.168.178.49 57684 typ host".into(),
            sdp_m_line_index: Some(0),
            sdp_mid: Some("0".into()),
            message_type: None,
            username_fragment: Some("eSGS".into()),
        };
        let body = ice_post_body(&[c]);
        let list = body["candidates"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        let first: Value = serde_json::from_str(list[0].as_str().unwrap()).unwrap();
        assert_eq!(first["candidate"], "candidate:1 1 UDP 2015363327 192.168.178.49 57684 typ host");
        assert_eq!(first["sdpMid"], "0");
        assert_eq!(first["sdpMLineIndex"], 0);
        assert_eq!(first["usernameFragment"], "eSGS");
        assert_eq!(list[1], "a=end-of-candidates");
        assert!(body.get("messageType").is_none());
    }

    #[test]
    fn parses_sdp_exchange() {
        let r: ExchangeResponse = serde_json::from_value(json!({
            "exchangeResponse": "{\"sdp\":\"v=0\\r\\n\",\"sdpType\":\"answer\",\"status\":\"success\"}",
            "errorDetails": null
        }))
        .unwrap();
        let a: SdpAnswer = serde_json::from_str(&r.exchange_response.unwrap()).unwrap();
        assert_eq!(a.sdp, "v=0\r\n");
    }
}
