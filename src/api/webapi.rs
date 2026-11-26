//! Xbox Live web APIs: profile, people (friends) and SmartGlass console list.

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::XstsToken;
use crate::http::Http;

#[derive(Clone)]
pub struct WebApi {
    http: Http,
    auth: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Console {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub console_type: String,
    #[serde(default)]
    pub power_state: String,
    #[serde(default)]
    pub console_streaming_enabled: bool,
    #[serde(default)]
    pub remote_management_enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Friend {
    pub xuid: String,
    #[serde(default)]
    pub gamertag: String,
    #[serde(default)]
    pub presence_state: String,
    #[serde(default)]
    pub presence_text: String,
}

impl WebApi {
    pub fn new(http: Http, web_token: &XstsToken) -> Self {
        Self { http, auth: web_token.xbl3_header() }
    }

    fn headers(&self, contract: &str) -> Vec<(&'static str, String)> {
        vec![
            ("Authorization", self.auth.clone()),
            ("x-xbl-contract-version", contract.to_string()),
            ("x-xbl-client-name", "XboxApp".into()),
            ("x-xbl-client-type", "UWA".into()),
            ("x-xbl-client-version", "39.39.22001.0".into()),
            ("Accept-Language", "en-US".into()),
        ]
    }

    pub async fn consoles(&self) -> Result<Vec<Console>> {
        let mut headers = self.headers("4");
        headers.push(("skillplatform", "RemoteManagement".into()));
        let v = self
            .http
            .get_json(
                "https://xccs.xboxlive.com/lists/devices?queryCurrentDevice=false&includeStorageDevices=true",
                &headers,
            )
            .await?;
        Ok(serde_json::from_value(v.get("result").cloned().unwrap_or(Value::Array(vec![])))?)
    }

    /// Returns `(setting id, value)` pairs for gamertag, gamerscore and picture.
    pub async fn profile(&self) -> Result<Vec<(String, String)>> {
        let v = self
            .http
            .get_json(
                "https://profile.xboxlive.com/users/me/profile/settings?settings=GameDisplayName,GameDisplayPicRaw,Gamerscore,Gamertag",
                &self.headers("2"),
            )
            .await?;
        let settings = v
            .pointer("/profileUsers/0/settings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(settings
            .into_iter()
            .filter_map(|s| {
                Some((s.get("id")?.as_str()?.to_string(), s.get("value")?.as_str()?.to_string()))
            })
            .collect())
    }

    pub async fn friends(&self) -> Result<Vec<Friend>> {
        let v = self
            .http
            .get_json(
                "https://peoplehub.xboxlive.com/users/me/people/social/decoration/preferredcolor,detail,multiplayersummary,presencedetail",
                &self.headers("3"),
            )
            .await?;
        Ok(serde_json::from_value(v.get("people").cloned().unwrap_or(Value::Array(vec![])))?)
    }
}
