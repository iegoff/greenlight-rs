//! Thin HTTP helper around `reqwest` with Greenlight-style default headers.

use anyhow::{anyhow, Result};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    /// Extra headers sent with every request (e.g. `X-Forwarded-For` for region spoofing).
    default_headers: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
#[error("HTTP {status} from {url}: {body}")]
pub struct HttpError {
    pub status: StatusCode,
    pub url: String,
    pub body: String,
}

impl Http {
    pub fn new(force_region_ip: Option<&str>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("greenlight-rs/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let mut default_headers = Vec::new();
        if let Some(ip) = force_region_ip.map(str::trim).filter(|s| !s.is_empty()) {
            default_headers.push(("X-Forwarded-For".to_string(), ip.to_string()));
        }
        Ok(Self { client, default_headers })
    }

    fn request(&self, method: Method, url: &str, headers: &[(&str, String)]) -> reqwest::RequestBuilder {
        let mut req = self.client.request(method, url);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        for (k, v) in &self.default_headers {
            req = req.header(k, v);
        }
        req
    }

    /// Sends a request and returns `(status, json-or-null)`. Non-2xx statuses
    /// are returned as [`HttpError`].
    pub async fn send(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, String)],
        json_body: Option<&Value>,
    ) -> Result<(StatusCode, Value)> {
        let mut req = self.request(method, url, headers).header("Accept", "application/json");
        if let Some(body) = json_body {
            req = req.json(body);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(HttpError { status, url: url.to_string(), body: text }.into());
        }
        let value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        Ok((status, value))
    }

    pub async fn get_json(&self, url: &str, headers: &[(&str, String)]) -> Result<Value> {
        Ok(self.send(Method::GET, url, headers, None).await?.1)
    }

    pub async fn post_json(&self, url: &str, body: &Value, headers: &[(&str, String)]) -> Result<Value> {
        Ok(self.send(Method::POST, url, headers, Some(body)).await?.1)
    }

    /// Posts an `application/x-www-form-urlencoded` body; returns `Err((status, body))`
    /// as an inner result for non-2xx responses so callers can inspect OAuth errors.
    pub async fn post_form_raw<T: Serialize + ?Sized>(
        &self,
        url: &str,
        form: &T,
    ) -> Result<std::result::Result<Value, (StatusCode, Value)>> {
        let resp = self
            .request(Method::POST, url, &[("Cache-Control", "no-store, must-revalidate, no-cache".into())])
            .form(form)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok(if status.is_success() { Ok(value) } else { Err((status, value)) })
    }

    pub async fn post_form<T: Serialize + ?Sized>(&self, url: &str, form: &T) -> Result<Value> {
        self.post_form_raw(url, form)
            .await?
            .map_err(|(status, body)| anyhow!("HTTP {status} from {url}: {body}"))
    }
}
