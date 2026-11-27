//! Shared application context for the CLI and the GUI: HTTP client, sign-in
//! state and helpers to build the streaming/web API clients.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::api::webapi::WebApi;
use crate::api::xcloud::{StreamKind, XCloudApi};
use crate::auth::{Msal, StreamingToken, TokenStore};
use crate::config::{self, Settings};
use crate::http::Http;

#[derive(Clone)]
pub struct App {
    pub http: Http,
    pub msal: Arc<Msal>,
    pub locale: String,
    pub settings: Settings,
}

impl App {
    /// Builds the context from settings plus optional CLI overrides.
    pub fn new(force_region_ip: Option<String>, locale: Option<String>) -> Result<Self> {
        let settings = Settings::load().unwrap_or_default();
        let force_ip = force_region_ip.or(settings.force_region_ip.clone());
        let locale = locale.or(settings.locale.clone()).unwrap_or_else(|| "en-US".into());
        let http = Http::new(force_ip.as_deref())?;
        let store = TokenStore::load(config::token_path()?)?;
        Ok(Self { msal: Arc::new(Msal::new(http.clone(), store)), http, locale, settings })
    }

    pub async fn require_login(&self) -> Result<()> {
        if !self.msal.has_user_token().await {
            return Err(anyhow!("not signed in, run `greenlight login` first"));
        }
        Ok(())
    }

    pub async fn web(&self) -> Result<WebApi> {
        self.require_login().await?;
        Ok(WebApi::new(self.http.clone(), &self.msal.web_token().await?))
    }

    pub async fn home_api(&self) -> Result<XCloudApi> {
        self.require_login().await?;
        let tokens = self.msal.streaming_tokens().await?;
        api_for(&self.http, &tokens.xhome, None, StreamKind::Home, &self.locale)
    }

    pub async fn cloud_api(&self, region: Option<&str>) -> Result<(XCloudApi, StreamingToken)> {
        self.require_login().await?;
        let tokens = self.msal.streaming_tokens().await?;
        let token = tokens
            .xcloud
            .ok_or_else(|| anyhow!("this account has no xCloud access (Game Pass Ultimate / supported region required)"))?;
        Ok((api_for(&self.http, &token, region, StreamKind::Cloud, &self.locale)?, token))
    }

    /// Imports the sign-in of an installed Greenlight desktop app. Returns
    /// the file used and the gamertag. A failed import keeps the previous
    /// sign-in.
    pub async fn import_greenlight(&self, from: Option<PathBuf>) -> Result<(PathBuf, String)> {
        let candidates = match from {
            Some(p) => vec![p],
            None => crate::auth::greenlight_store_candidates(),
        };
        let found = candidates.iter().find_map(|p| match std::fs::read_to_string(p) {
            Ok(c) => Some((p.clone(), c)),
            Err(e) => {
                tracing::debug!("{}: {e}", p.display());
                None
            }
        });
        let Some((path, contents)) = found else {
            let list: Vec<String> = candidates.iter().map(|p| format!("  {}", p.display())).collect();
            return Err(anyhow!(
                "no Greenlight settings found. Looked in:\n{}\nUse --from <path to Greenlight's config.json>.",
                list.join("\n")
            ));
        };
        let token = crate::auth::parse_greenlight_tokens(&contents)?;
        let previous = self.msal.user_token().await;
        self.msal.import_user_token(token).await?;
        match self.msal.web_token().await {
            Ok(web) => Ok((path, web.gamertag().unwrap_or("<unknown>").to_string())),
            Err(e) => {
                // Don't leave a broken sign-in behind: restore what we had.
                match previous {
                    Some(prev) => self.msal.import_user_token(prev).await?,
                    None => self.msal.logout().await?,
                }
                Err(e.context("the imported sign-in didn't work (your previous sign-in was kept); try signing in instead"))
            }
        }
    }
}

pub fn api_for(
    http: &Http,
    token: &StreamingToken,
    region: Option<&str>,
    kind: StreamKind,
    locale: &str,
) -> Result<XCloudApi> {
    let region = match region {
        Some(name) => token.region(name).ok_or_else(|| anyhow!("unknown region {name}"))?,
        None => token.default_region().ok_or_else(|| anyhow!("streaming token has no regions"))?,
    };
    tracing::info!("using {} region {} ({})", kind.as_str(), region.name, region.base_uri);
    Ok(XCloudApi::new(http.clone(), &region.base_uri, &token.gs_token, kind, locale))
}
