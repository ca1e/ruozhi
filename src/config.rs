//! Optional, read-only config file (`~/.config/ruozhi/config.toml`, or
//! `%APPDATA%\ruozhi\config.toml` on Windows):
//! present -> its values are used; absent -> nothing is ever written and the
//! defaults apply (hardware-derived identity + OTA-issued credentials).
//! Create it by hand only to pin a device identity or use a custom server.

use crate::identity;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Official xiaozhi websocket endpoint (as handed out by the OTA endpoint).
pub const DEFAULT_WS_URL: &str = "wss://api.tenclass.net/xiaozhi/v1/";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Log file path; default /tmp/ruozhi.log (env RUOZHI_LOG_FILE wins).
    #[serde(default)]
    pub log_file: Option<String>,
    /// Opt into binary protocol v2 (per-frame timestamps) so the SERVER can
    /// echo-cancel; CLI --server-aec also enables it.
    #[serde(default)]
    pub server_aec: Option<bool>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    /// Defaults to this machine's MAC; pin it here to mimic a specific device.
    #[serde(default)]
    pub device_id: Option<String>,
    /// Defaults to a stable UUIDv4 derived from the machine; pin to override.
    #[serde(default)]
    pub client_id: Option<String>,
}

impl Config {
    /// Platform config location: `%APPDATA%\ruozhi\config.toml` on Windows,
    /// `~/.config/ruozhi/config.toml` elsewhere (XDG default).
    pub fn path() -> PathBuf {
        #[cfg(windows)]
        {
            let base = std::env::var("APPDATA")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| ".".into());
            PathBuf::from(base).join("ruozhi").join("config.toml")
        }
        #[cfg(not(windows))]
        {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config/ruozhi/config.toml")
        }
    }

    /// Missing or unparsable file simply means "all defaults".
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str(&s) {
                Ok(c) => {
                    log::info!("config: {}", path.display());
                    c
                }
                Err(e) => {
                    log::warn!("config {} ignored (parse error: {e}); using defaults", path.display());
                    Self::default()
                }
            },
            Err(_) => {
                log::info!("no config file at {} — using defaults", path.display());
                Self::default()
            }
        }
    }
}

/// Resolved connection settings passed to the protocol thread.
pub struct Connection {
    pub url: String,
    pub token: String,
    pub device_id: String,
    pub client_id: String,
    /// Binary protocol v2 (timestamped frames) so the server can AEC.
    pub server_aec: bool,
    /// Speaker volume handle shared with the playback path (MCP tools).
    pub volume: crate::audio::Volume,
}

/// Which mode the binary should run in.
pub enum Mode {
    /// Normal hold-to-talk client.
    App,
    /// UI phase cycler.
    Demo,
    /// Mic -> opus -> decode -> speaker self test.
    Loopback,
    /// Headless BMP dump of each UI phase.
    RenderFrames(PathBuf),
    /// Full client, but the talk audio comes from a 16-bit PCM WAV file.
    Wav(PathBuf),
}

/// Fully resolved runtime settings.
pub struct Args {
    pub config: Config,
    pub url_override: Option<String>,
    pub token_override: Option<String>,
    pub server_aec: bool,
    pub mode: Mode,
}

pub fn parse() -> Args {
    let mut config = Config::load();
    let mut mode = Mode::App;
    let mut url_override = None;
    let mut token_override = None;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut val = |name: &str| -> String {
            argv.next().unwrap_or_else(|| panic!("{name} needs a value"))
        };
        match arg.as_str() {
            "--url" => url_override = Some(val("--url")),
            "--token" => token_override = Some(val("--token")),
            "--device-id" => config.device_id = Some(val("--device-id")),
            "--server-aec" => config.server_aec = Some(true),
            "--client-id" => config.client_id = Some(val("--client-id")),
            "--demo" => mode = Mode::Demo,
            "--loopback" => mode = Mode::Loopback,
            "--render-frames" => mode = Mode::RenderFrames(PathBuf::from(val("--render-frames"))),
            "--wav" => mode = Mode::Wav(PathBuf::from(val("--wav"))),
            other => {
                eprintln!(
                    "usage: ruozhi [--url U] [--token T] [--device-id D] [--client-id C]\n\
                     \x20       ruozhi --demo | --loopback | --render-frames <dir> | --wav <file>"
                );
                panic!("unknown argument: {other}");
            }
        }
    }
    let server_aec = config.server_aec.unwrap_or(false);
    Args {
        config,
        url_override,
        token_override,
        server_aec,
        mode,
    }
}

/// Resolve the effective settings: CLI args > config file > defaults
/// (hardware identity; credentials from OTA; official endpoint). No file is
/// ever written.
pub fn finalize(config: Config, args: &Args, volume: crate::audio::Volume) -> Result<Connection> {
    // ids: CLI / config file > hardware identity
    let device_id = config
        .device_id
        .clone()
        .unwrap_or_else(identity::device_id);
    let client_id = config
        .client_id
        .clone()
        .unwrap_or_else(identity::client_id);
    log::info!("device_id: {device_id}");
    log::info!("client_id: {client_id}");

    // url/token: CLI override > config file > OTA bootstrap > default url
    let (mut url, mut token) = (args.url_override.clone(), args.token_override.clone());
    if url.is_none() && token.is_none() {
        if let (Some(u), Some(t)) = (&config.url, &config.token) {
            log::info!("using url/token from the config file");
            url = Some(u.clone());
            token = Some(t.clone());
        }
    }
    if (url.is_none() || token.is_none())
        && args.url_override.is_none()
        && args.token_override.is_none()
    {
        log::info!(
            "fetching connection settings from {} (like a real device)",
            crate::ota::OTA_URL
        );
        let fetched = crate::ota::fetch(&device_id, &client_id)
            .map_err(|e| anyhow::anyhow!("OTA bootstrap failed: {e:#}"))?;
        url = Some(url.unwrap_or(fetched.url));
        token = Some(token.unwrap_or(fetched.token));
        if let Some(code) = fetched.activation_code {
            log::info!(
                "device not bound yet: activation code {code} — hold {} and talk, \
                 the server will announce it",
                crate::fenster::talk_key_name()
            );
        }
    }

    Ok(Connection {
        server_aec: args.server_aec,
        volume,
        url: url.unwrap_or_else(|| DEFAULT_WS_URL.to_string()),
        token: token
            .context("no access token: OTA did not provide one and --token was not given")?,
        device_id,
        client_id,
    })
}
