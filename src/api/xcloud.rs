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
        }
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
        Ok(self.http.send(Method::POST, &self.url(path), &headers, Some(body)).await?.1)
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
            "clientSessionId": "",
            "settings": {
                "nanoVersion": "V3;WebrtcTransport.dll",
                "enableTextToSpeech": false,
                "enableOptionalDataCollection": false,
                "highContrast": 0,
                "locale": self.locale,
                "useIceConnection": false,
                "timezoneOffsetMinutes": timezone_offset_minutes(),
                "sdkType": "web",
                "osName": "windows",
            },
            "serverId": if self.kind == StreamKind::Home { target } else { "" },
            "fallbackRegionNames": [],
        });
        let resp = self
            .post(
                &format!("v5/sessions/{}/play", self.kind.as_str()),
                &body,
                &[("X-MS-Device-Info", device_info())],
            )
            .await
            .context("starting stream session")?;
        Ok(serde_json::from_value(resp)?)
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
            "requestId": "1",
            "configuration": {
                "chatConfiguration": {
                    "bytesPerSample": 2,
                    "expectedClipDurationMs": 20,
                    "format": { "codec": "opus", "container": "webm" },
                    "numChannels": 1,
                    "sampleFrequencyHz": 24000,
                },
                "chat": { "minVersion": 1, "maxVersion": 1 },
                "control": { "minVersion": 1, "maxVersion": 3 },
                "input": { "minVersion": 1, "maxVersion": 9 },
                "message": { "minVersion": 1, "maxVersion": 1 },
                "reliableinput": { "minVersion": 9, "maxVersion": 9 },
                "unreliableinput": { "minVersion": 9, "maxVersion": 9 },
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
        let body = json!({ "messageType": "iceCandidate", "candidate": candidates });
        let path = self.session(session_id, "/ice");
        self.post(&path, &body, &[]).await.context("posting ICE candidates")?;
        let resp: ExchangeResponse =
            serde_json::from_value(self.get_until_ready(&path, Duration::from_secs(30)).await?)?;
        let exchange = resp.exchange_response.ok_or_else(|| anyhow!("ICE exchange returned no candidates"))?;
        serde_json::from_str(&exchange).context("parsing remote ICE candidates")
    }
}

fn timezone_offset_minutes() -> i32 {
    use chrono::Local;
    Local::now().offset().local_minus_utc() / 60
}

/// Device description sent with `/play`. Mirrors the Xbox app on Windows.
pub fn device_info() -> String {
    json!({
        "appInfo": {
            "env": {
                "clientAppId": "Microsoft.GamingApp",
                "clientAppType": "native",
                "clientAppVersion": "2203.1001.4.0",
                "clientSdkVersion": "8.5.2",
                "httpEnvironment": "prod",
                "sdkInstallId": "",
            }
        },
        "dev": {
            "hw": { "make": "Microsoft", "model": "Surface Pro", "sdktype": "native" },
            "os": { "name": "Windows 11", "ver": "22631.2715", "platform": "desktop" },
            "displayInfo": {
                "dimensions": { "widthInPixels": 1920, "heightInPixels": 1080 },
                "pixelDensity": { "dpiX": 1, "dpiY": 1 },
            },
        },
    })
    .to_string()
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
