//! Bootstrap connection settings from the xiaozhi OTA endpoint, exactly like
//! a real device on first boot: POST device info, receive websocket
//! url/token (and an activation code until the device is bound at
//! xiaozhi.me).

use anyhow::{Context, Result};
use serde::Deserialize;

pub const OTA_URL: &str = "https://api.tenclass.net/xiaozhi/ota/";

#[derive(Deserialize)]
struct OtaResponse {
    websocket: Option<OtaWebsocket>,
    activation: Option<OtaActivation>,
}

#[derive(Deserialize)]
struct OtaWebsocket {
    #[serde(default)]
    url: String,
    #[serde(default)]
    token: String,
}

#[derive(Deserialize)]
struct OtaActivation {
    #[serde(default)]
    code: String,
}

pub struct OtaConfig {
    pub url: String,
    pub token: String,
    /// 6-digit code while the device is unbound. The official server speaks
    /// it in its reply when an unbound device talks, and the user enters it
    /// at xiaozhi.me to bind the device.
    pub activation_code: Option<String>,
}

pub fn fetch(device_id: &str, client_id: &str) -> Result<OtaConfig> {
    let body = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "language": "zh-CN",
        "mac_address": device_id,
        "uuid": client_id,
        "board": { "type": "ruozhi", "name": "ruozhi-macos" },
        "application": { "name": "ruozhi", "version": env!("CARGO_PKG_VERSION") },
    });

    let resp = ureq::post(OTA_URL)
        .set("Content-Type", "application/json")
        .set("Device-Id", device_id)
        .set("Client-Id", client_id)
        .set("Activation-Version", "1")
        .set("User-Agent", concat!("ruozhi/", env!("CARGO_PKG_VERSION")))
        .set("Accept-Language", "zh-CN")
        .send_string(&body.to_string())
        .map_err(|e| anyhow::anyhow!("OTA request failed: {e}"))?;

    let text = resp
        .into_string()
        .context("reading OTA response body")?;
    let parsed: OtaResponse =
        serde_json::from_str(&text).context("parsing OTA response")?;

    let ws = parsed.websocket.context("OTA response has no websocket config")?;
    let activation_code = parsed
        .activation
        .map(|a| a.code)
        .filter(|c| !c.is_empty());
    Ok(OtaConfig {
        url: ws.url,
        token: ws.token,
        activation_code,
    })
}
