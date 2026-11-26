//! Microsoft account / Xbox Live authentication.
//!
//! Port of the `Msal` flow from `xal-node` used by Greenlight:
//!
//! 1. OAuth2 device-code login against `login.microsoftonline.com`
//!    (scope `xboxlive.signin openid profile offline_access`).
//! 2. User token → Xbox Live user token (`user.auth.xboxlive.com`).
//! 3. XSTS authorization for `http://xboxlive.com` (web API) and
//!    `http://gssv.xboxlive.com/` (game streaming).
//! 4. GSSV token → streaming tokens for `xhome` and `xgpuweb`/`xgpuwebf2p`.
//! 5. For home streaming, a console-transfer token from `login.live.com`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::http::Http;

pub const CLIENT_ID: &str = "1f907974-e22b-4810-a9de-d9647380c97e";
const SCOPE: &str = "xboxlive.signin openid profile offline_access";
const TRANSFER_SCOPE: &str =
    "service::http://Passport.NET/purpose::PURPOSE_XBOX_CLOUD_CONSOLE_TRANSFER_TOKEN";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserToken {
    pub token_type: String,
    pub expires_in: i64,
    pub scope: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_on: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ext_expires_in: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

impl UserToken {
    pub fn seconds_valid(&self) -> i64 {
        self.expires_on.map(|e| (e - Utc::now()).num_seconds()).unwrap_or(-1)
    }

    pub fn is_valid(&self) -> bool {
        self.seconds_valid() > 0
    }
}

/// Persistent token store, JSON compatible with `xal-node`'s `TokenStore`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenStore {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_token: Option<UserToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sisu_token: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwt_keys: Option<Value>,
    #[serde(skip)]
    path: PathBuf,
}

impl TokenStore {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut store: TokenStore = match std::fs::read_to_string(&path) {
            Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s)
                .with_context(|| format!("parsing token store {}", path.display()))?,
            _ => TokenStore::default(),
        };
        store.path = path;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        write_private(&self.path, data.as_bytes())
            .with_context(|| format!("writing token store {}", self.path.display()))
    }

    pub fn set_user_token(&mut self, mut token: UserToken) {
        token.expires_on = Some(Utc::now() + Duration::seconds(token.expires_in));
        self.user_token = Some(token);
    }

    pub fn clear(&mut self) -> Result<()> {
        self.user_token = None;
        self.sisu_token = None;
        self.jwt_keys = None;
        self.save()
    }
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeResponse {
    pub user_code: String,
    pub device_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
    pub message: String,
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XstsToken {
    #[serde(rename = "IssueInstant")]
    pub issue_instant: DateTime<Utc>,
    #[serde(rename = "NotAfter")]
    pub not_after: DateTime<Utc>,
    #[serde(rename = "Token")]
    pub token: String,
    #[serde(rename = "DisplayClaims")]
    pub display_claims: Value,
}

impl XstsToken {
    pub fn seconds_valid(&self) -> i64 {
        (self.not_after - Utc::now()).num_seconds()
    }

    pub fn user_hash(&self) -> Option<&str> {
        self.display_claims.pointer("/xui/0/uhs").and_then(Value::as_str)
    }

    pub fn gamertag(&self) -> Option<&str> {
        self.display_claims.pointer("/xui/0/gtg").and_then(Value::as_str)
    }

    pub fn xuid(&self) -> Option<&str> {
        self.display_claims.pointer("/xui/0/xid").and_then(Value::as_str)
    }

    /// `Authorization` header value for Xbox Live web APIs.
    pub fn xbl3_header(&self) -> String {
        format!("XBL3.0 x={};{}", self.user_hash().unwrap_or_default(), self.token)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Region {
    pub name: String,
    pub base_uri: String,
    #[serde(default)]
    pub network_test_hostname: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub fallback_priority: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferingSettings {
    #[serde(default)]
    pub allow_region_selection: bool,
    #[serde(default)]
    pub regions: Vec<Region>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamingToken {
    pub offering_settings: OfferingSettings,
    pub market: String,
    pub gs_token: String,
    pub token_type: String,
    pub duration_in_seconds: i64,
    #[serde(skip, default = "Utc::now")]
    pub created: DateTime<Utc>,
}

impl StreamingToken {
    pub fn seconds_valid(&self) -> i64 {
        (self.created + Duration::seconds(self.duration_in_seconds) - Utc::now()).num_seconds()
    }

    pub fn default_region(&self) -> Option<&Region> {
        let regions = &self.offering_settings.regions;
        regions.iter().find(|r| r.is_default).or_else(|| regions.first())
    }

    pub fn region(&self, name: &str) -> Option<&Region> {
        self.offering_settings.regions.iter().find(|r| r.name.eq_ignore_ascii_case(name))
    }
}

#[derive(Debug, Clone)]
pub struct StreamingTokens {
    pub xhome: StreamingToken,
    pub xcloud: Option<StreamingToken>,
}

/// Console-transfer token used to `/connect` home streaming sessions.
#[derive(Debug, Clone)]
pub struct MsalToken {
    pub lpt: String,
    pub refresh_token: Option<String>,
    pub user_id: Option<String>,
}

pub struct Msal {
    http: Http,
    store: Mutex<TokenStore>,
    xsts: Mutex<Option<XstsToken>>,
    gssv: Mutex<Option<XstsToken>>,
}

impl Msal {
    pub fn new(http: Http, store: TokenStore) -> Self {
        Self { http, store: Mutex::new(store), xsts: Mutex::new(None), gssv: Mutex::new(None) }
    }

    pub async fn has_user_token(&self) -> bool {
        self.store.lock().await.user_token.is_some()
    }

    pub async fn logout(&self) -> Result<()> {
        self.store.lock().await.clear()
    }

    pub async fn start_device_code(&self) -> Result<DeviceCodeResponse> {
        let resp = self
            .http
            .post_form(
                "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode",
                &[("client_id", CLIENT_ID), ("scope", SCOPE)],
            )
            .await?;
        Ok(serde_json::from_value(resp)?)
    }

    /// Polls until the user completed the device-code login (or it expired).
    pub async fn poll_device_code(&self, code: &DeviceCodeResponse) -> Result<UserToken> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(code.expires_in);
        let mut interval = code.interval.max(1);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            if std::time::Instant::now() > deadline {
                bail!("device code expired before login completed");
            }
            let res = self
                .http
                .post_form_raw(
                    "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
                    &[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ("client_id", CLIENT_ID),
                        ("device_code", &code.device_code),
                    ],
                )
                .await?;
            match res {
                Ok(body) => {
                    let token: UserToken = serde_json::from_value(body)?;
                    let mut store = self.store.lock().await;
                    store.sisu_token = None;
                    store.jwt_keys = None;
                    store.set_user_token(token.clone());
                    store.save()?;
                    return Ok(token);
                }
                Err((_, body)) => {
                    let err = body.get("error").and_then(Value::as_str).unwrap_or_default();
                    match err {
                        "authorization_pending" => continue,
                        "slow_down" => interval += 5,
                        "" => continue,
                        other => bail!(
                            "device code login failed: {other}: {}",
                            body.get("error_description").and_then(Value::as_str).unwrap_or("")
                        ),
                    }
                }
            }
        }
    }

    pub async fn refresh_user_token(&self) -> Result<UserToken> {
        let refresh = {
            let store = self.store.lock().await;
            store
                .user_token
                .as_ref()
                .map(|t| t.refresh_token.clone())
                .ok_or_else(|| anyhow!("not logged in, run `greenlight login` first"))?
        };
        let body = self
            .http
            .post_form_raw(
                "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
                &[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &refresh),
                    ("scope", SCOPE),
                ],
            )
            .await?
            .map_err(|(status, body)| {
                anyhow!("refreshing user token failed ({status}): {body}. Please run `greenlight login` again.")
            })?;
        let token: UserToken = serde_json::from_value(body)?;
        let mut store = self.store.lock().await;
        store.set_user_token(token);
        store.save()?;
        Ok(store.user_token.clone().expect("just set"))
    }

    pub async fn user_access_token(&self) -> Result<String> {
        let current = self.store.lock().await.user_token.clone();
        match current {
            Some(t) if t.seconds_valid() > 60 => Ok(t.access_token),
            _ => Ok(self.refresh_user_token().await?.access_token),
        }
    }

    async fn xbox_user_token(&self) -> Result<XstsToken> {
        let mut cached = self.xsts.lock().await;
        if let Some(t) = cached.as_ref().filter(|t| t.seconds_valid() > 60) {
            return Ok(t.clone());
        }
        let access = self.user_access_token().await?;
        let body = json!({
            "Properties": {
                "AuthMethod": "RPS",
                "RpsTicket": format!("d={access}"),
                "SiteName": "user.auth.xboxlive.com",
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT",
        });
        let resp = self
            .http
            .post_json(
                "https://user.auth.xboxlive.com/user/authenticate",
                &body,
                &xbl_auth_headers(),
            )
            .await
            .context("Xbox Live user authentication")?;
        let token: XstsToken = serde_json::from_value(resp)?;
        *cached = Some(token.clone());
        Ok(token)
    }

    pub async fn xsts_authorize(&self, relying_party: &str) -> Result<XstsToken> {
        let user = self.xbox_user_token().await?;
        let body = json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [user.token] },
            "RelyingParty": relying_party,
            "TokenType": "JWT",
        });
        let mut headers = xbl_auth_headers();
        headers.push(("Accept", "*/*".into()));
        headers.push(("ms-cv", "0".into()));
        let resp = self
            .http
            .post_json("https://xsts.auth.xboxlive.com/xsts/authorize", &body, &headers)
            .await
            .with_context(|| format!("XSTS authorization for {relying_party}"))?;
        Ok(serde_json::from_value(resp)?)
    }

    /// Token for the Xbox Live web APIs (profile, people, smartglass, ...).
    pub async fn web_token(&self) -> Result<XstsToken> {
        self.xsts_authorize("http://xboxlive.com").await
    }

    pub async fn gssv_token(&self) -> Result<XstsToken> {
        {
            let cached = self.gssv.lock().await;
            if let Some(t) = cached.as_ref().filter(|t| t.seconds_valid() > 60) {
                return Ok(t.clone());
            }
        }
        let token = self.xsts_authorize("http://gssv.xboxlive.com/").await?;
        *self.gssv.lock().await = Some(token.clone());
        Ok(token)
    }

    pub async fn stream_token(&self, gssv: &str, offering: &str) -> Result<StreamingToken> {
        let body = json!({ "token": gssv, "offeringId": offering });
        let headers = vec![
            ("Cache-Control", "no-store, must-revalidate, no-cache".to_string()),
            ("x-gssv-client", "XboxComBrowser".to_string()),
        ];
        let resp = self
            .http
            .post_json(
                &format!("https://{offering}.gssv-play-prod.xboxlive.com/v2/login/user"),
                &body,
                &headers,
            )
            .await
            .with_context(|| format!("streaming login for offering {offering}"))?;
        Ok(serde_json::from_value(resp)?)
    }

    pub async fn streaming_tokens(&self) -> Result<StreamingTokens> {
        let gssv = self.gssv_token().await?;
        let xhome = self.stream_token(&gssv.token, "xhome").await?;
        let xcloud = match self.stream_token(&gssv.token, "xgpuweb").await {
            Ok(t) => Some(t),
            Err(_) => match self.stream_token(&gssv.token, "xgpuwebf2p").await {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::info!("no xCloud streaming token (unsupported region?): {e:#}");
                    None
                }
            },
        };
        Ok(StreamingTokens { xhome, xcloud })
    }

    /// Console-transfer token needed by home streaming in `ReadyToConnect`.
    pub async fn msal_token(&self) -> Result<MsalToken> {
        // Ensure the stored refresh token is fresh.
        self.user_access_token().await?;
        let refresh = self
            .store
            .lock()
            .await
            .user_token
            .as_ref()
            .map(|t| t.refresh_token.clone())
            .ok_or_else(|| anyhow!("not logged in"))?;
        let body = self
            .http
            .post_form(
                "https://login.live.com/oauth20_token.srf",
                &[
                    ("client_id", CLIENT_ID),
                    ("scope", TRANSFER_SCOPE),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &refresh),
                ],
            )
            .await
            .context("fetching console transfer token")?;
        Ok(MsalToken {
            lpt: body
                .get("access_token")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("no access_token in transfer token response"))?
                .to_string(),
            refresh_token: body.get("refresh_token").and_then(Value::as_str).map(String::from),
            user_id: body.get("user_id").and_then(Value::as_str).map(String::from),
        })
    }
}

fn xbl_auth_headers() -> Vec<(&'static str, String)> {
    vec![
        ("x-xbl-contract-version", "1".into()),
        ("Cache-Control", "no-cache".into()),
        ("Origin", "https://www.xbox.com".into()),
        ("Referer", "https://www.xbox.com/".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_store_roundtrip_is_xal_compatible() {
        let dir = std::env::temp_dir().join(format!("greenlight-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("tokens.json");
        let mut store = TokenStore::load(&path).unwrap();
        assert!(store.user_token.is_none());
        store.set_user_token(UserToken {
            token_type: "bearer".into(),
            expires_in: 3600,
            scope: "XboxLive.signin".into(),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            user_id: None,
            expires_on: None,
            ext_expires_in: None,
            id_token: None,
        });
        store.save().unwrap();

        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["userToken"]["refresh_token"], "rt");
        assert!(raw["userToken"]["expires_on"].is_string());

        let loaded = TokenStore::load(&path).unwrap();
        let t = loaded.user_token.unwrap();
        assert!(t.seconds_valid() > 3500);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn parses_streaming_token() {
        let t: StreamingToken = serde_json::from_value(json!({
            "offeringSettings": {
                "allowRegionSelection": false,
                "regions": [
                    {"name": "WestEurope", "baseUri": "https://weu.core.gssv-play-prod.xboxlive.com", "isDefault": false},
                    {"name": "UkSouth", "baseUri": "https://uks.core.gssv-play-prod.xboxlive.com", "isDefault": true}
                ],
                "selectableServerTypes": null,
                "clientCloudSettings": {"Environments": []}
            },
            "market": "GB",
            "gsToken": "abc",
            "tokenType": "bearer",
            "durationInSeconds": 14400
        }))
        .unwrap();
        assert_eq!(t.default_region().unwrap().name, "UkSouth");
        assert_eq!(t.region("westeurope").unwrap().base_uri, "https://weu.core.gssv-play-prod.xboxlive.com");
        assert!(t.seconds_valid() > 14000);
    }

    #[test]
    fn xsts_claims() {
        let t: XstsToken = serde_json::from_value(json!({
            "IssueInstant": "2024-01-01T00:00:00.0000000Z",
            "NotAfter": "2099-01-01T00:00:00.0000000Z",
            "Token": "tok",
            "DisplayClaims": {"xui": [{"uhs": "123", "gtg": "Gamer", "xid": "2535"}]}
        }))
        .unwrap();
        assert_eq!(t.xbl3_header(), "XBL3.0 x=123;tok");
        assert_eq!(t.gamertag(), Some("Gamer"));
        assert!(t.seconds_valid() > 0);
    }
}
