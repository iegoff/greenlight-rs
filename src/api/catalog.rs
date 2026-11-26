//! Game Pass catalog lookups (title names for xCloud title ids).

use std::collections::HashMap;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::http::Http;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Product {
    #[serde(default)]
    pub product_title: String,
    #[serde(default)]
    pub publisher_name: String,
    #[serde(default, rename = "XCloudTitleId")]
    pub xcloud_title_id: Option<String>,
    #[serde(default)]
    pub store_id: Option<String>,
}

/// Cloud title as returned by `/v2/titles`.
#[derive(Debug, Clone)]
pub struct CloudTitle {
    pub title_id: String,
    pub product_id: Option<String>,
    pub has_entitlement: bool,
    pub name: Option<String>,
}

pub fn parse_titles(v: &Value) -> Vec<CloudTitle> {
    v.get("results")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    Some(CloudTitle {
                        title_id: t.get("titleId")?.as_str()?.to_string(),
                        product_id: t
                            .pointer("/details/productId")
                            .and_then(Value::as_str)
                            .map(String::from),
                        has_entitlement: t
                            .pointer("/details/hasEntitlement")
                            .and_then(Value::as_bool)
                            .unwrap_or(true),
                        name: t.pointer("/details/name").and_then(Value::as_str).map(String::from),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub async fn resolve_products(
    http: &Http,
    product_ids: &[String],
    market: &str,
    language: &str,
) -> Result<HashMap<String, Product>> {
    let mut out = HashMap::new();
    for batch in product_ids.chunks(100) {
        let url = format!(
            "https://catalog.gamepass.com/v3/products?market={market}&language={language}&hydration=RemoteHighSapphire0"
        );
        let v = http
            .post_json(
                &url,
                &json!({ "Products": batch }),
                &[
                    ("ms-cv", "0".into()),
                    ("calling-app-name", "Xbox Cloud Gaming Web".into()),
                    ("calling-app-version", "21.0.0".into()),
                ],
            )
            .await?;
        if let Some(products) = v.get("Products").and_then(Value::as_object) {
            for (id, p) in products {
                if let Ok(p) = serde_json::from_value::<Product>(p.clone()) {
                    out.insert(id.clone(), p);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        let v = json!({"results": [
            {"titleId": "HALO", "details": {"productId": "9PP5G1F0C2B6", "hasEntitlement": true}},
            {"titleId": "X"}
        ]});
        let t = parse_titles(&v);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].product_id.as_deref(), Some("9PP5G1F0C2B6"));
        assert!(t[1].has_entitlement);
    }
}
