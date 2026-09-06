//! The control thread: owns the WebSocket, runs the hold-to-talk state
//! machine and shuttles opus packets both ways.
//!
//! Wire format (xiaozhi WebSocket protocol, binary version 1):
//! - text frames are JSON (hello / listen / abort / tts / stt ...),
//! - binary frames are bare opus packets,
//! - every JSON message carries the `session_id` from the server hello.

use crate::audio::AudioHandles;
use crate::config::Connection;
use std::sync::atomic::Ordering as AtomicOrdering;
use crate::state::{Phase, SharedState, UiEvent};
use anyhow::{bail, Context, Result};
use crossbeam_channel::Receiver;
use http::HeaderValue;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Bytes, Message, WebSocket};

/// Binary protocol version advertised in the hello (1 = bare opus frames).
const PROTOCOL_VERSION: u32 = 1;
/// Give up if the server hello does not arrive in time (same as the device).
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Connection is dead after this long without any inbound data.
const INBOUND_TIMEOUT: Duration = Duration::from_secs(120);
/// Poll granularity while the socket is open (ws read timeout).
const READ_POLL: Duration = Duration::from_millis(50);

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

pub fn spawn(
    config: Connection,
    shared: Arc<SharedState>,
    ui_rx: Receiver<UiEvent>,
    audio: AudioHandles,
) {
    std::thread::Builder::new()
        .name("protocol".into())
        .spawn(move || {
            if let Err(e) = run(config, shared, ui_rx, audio) {
                log::error!("protocol thread stopped: {e:#}");
            }
        })
        .expect("spawn protocol thread");
}

struct Session {
    ws: Ws,
    session_id: String,
    last_inbound: Instant,
    /// negotiated binary protocol version (1 bare opus, 2 v2-framed + AEC)
    version: u32,
    /// uplink opus frames sent (for v2 timestamps)
    frames_sent: u64,
}

fn run(
    config: Connection,
    shared: Arc<SharedState>,
    ui_rx: Receiver<UiEvent>,
    audio: AudioHandles,
) -> Result<()> {
    let out_rate = audio.out_rate;
    let AudioHandles {
        ref encoded_rx,
        ref jitter,
        ref decode_rate,
        ..
    } = audio;

    let mut session: Option<Session> = None;
    let mut pressed = false;
    let mut last_ping = Instant::now();
    let mut up_frames: usize = 0;
    let mut last_stat = Instant::now();
    // Set when `tts stop` arrived: drain the audio tail, then disconnect.
    let mut draining = false;

    loop {
        // ---- UI events, drained no matter the connection state ----
        for ev in ui_rx.try_iter() {
            match ev {
                UiEvent::Quit => {
                    shared.set_mic_enabled(false);
                    if let Some(s) = session.as_mut() {
                        let _ = s.ws.send(Message::Close(None));
                        let _ = s.ws.flush();
                    }
                    return Ok(());
                }
                UiEvent::CmdDown => {
                    pressed = true;
                    on_cmd_down(&mut session, &mut draining, &config, &shared, &decode_rate, &audio);
                }
                UiEvent::CmdUp => {
                    pressed = false;
                    on_cmd_up(&mut session, &shared);
                }
            }
        }

        // If the button was released before the handshake finished, stop
        // immediately instead of listening open-mouthed.
        if shared.phase() == Phase::Listening && !pressed {
            on_cmd_up(&mut session, &shared);
        }

        // ---- closed: wait for the next press ----
        if session.is_none() {
            match ui_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(UiEvent::Quit) => return Ok(()),
                Ok(UiEvent::CmdDown) => {
                    pressed = true;
                    on_cmd_down(
                        &mut session,
                        &mut draining,
                        &config,
                        &shared,
                        &decode_rate,
                        &audio,
                    );
                }
                Ok(UiEvent::CmdUp) => pressed = false,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(()),
            }
            continue;
        }

        // ---- open: poll the socket ----
        let mut close_reason: Option<String> = None;
        let inbound_stale;
        {
            let Some(s) = session.as_mut() else {
                unreachable!("session was Some above");
            };
            match s.ws.read() {
            Ok(msg) => {
                s.last_inbound = Instant::now();
                match msg {
                    Message::Text(t) => {
                        match handle_json(
                            &t,
                            &mut s.session_id,
                            &config,
                            &shared,
                            &decode_rate,
                            &mut draining,
                        ) {
                            Ok(Some(reply)) => {
                                if let Err(e) = send_json(s, reply) {
                                    log::warn!("mcp reply failed: {e:#}");
                                }
                            }
                            Ok(None) => {}
                            Err(e) => log::warn!("bad server message: {e:#}"),
                        }
                    }
                    Message::Binary(b) => {
                        if draining {
                            continue;
                        }
                        // Downlink framing follows the negotiated version:
                        // v1 = bare opus, v2 = 16-byte header, v3 = 4-byte header.
                        match s.version {
                            2 if b.len() >= 16 => {
                                let v_type = u16::from_be_bytes([b[2], b[3]]);
                                match v_type {
                                    0 => jitter.push(b[16..].to_vec()),
                                    1 => {
                                        if let Ok(t) = std::str::from_utf8(&b[16..]) {
                                            if let Err(e) = handle_json(
                                                t,
                                                &mut s.session_id,
                                                &config,
                                                &shared,
                                                &decode_rate,
                                                &mut draining,
                                            ) {
                                                log::warn!("bad server message: {e:#}");
                                            }
                                        }
                                    }
                                    other => log::debug!("v2 type {other} ignored"),
                                }
                            }
                            3 if b.len() >= 4 => {
                                let t = u16::from_be_bytes([b[2], b[3]]);
                                jitter.push(b[4..4 + t as usize].to_vec());
                            }
                            _ => jitter.push(b.to_vec()),
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(cf) => {
                        log::info!("server closed: {cf:?}");
                        close_reason = Some("server closed".into());
                    }
                    Message::Frame(_) => {}
                }
            }
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => close_reason = Some(format!("websocket error: {e}")),
        }

        // ship encoded mic frames
        if shared.mic_enabled() {
            for pkt in encoded_rx.try_iter() {
                up_frames += 1;
                s.frames_sent += 1;
                let msg = if s.version >= 2 {
                    // BinaryProtocol v2 (big-endian): lets the SERVER align
                    // mic/playback for echo cancellation
                    let ts = (s.frames_sent * crate::audio::FRAME_MS as u64) as u32;
                    let mut frame =
                        Vec::with_capacity(16 + pkt.len());
                    frame.extend_from_slice(&2u16.to_be_bytes()); // version
                    frame.extend_from_slice(&0u16.to_be_bytes()); // type: opus
                    frame.extend_from_slice(&0u32.to_be_bytes()); // reserved
                    frame.extend_from_slice(&ts.to_be_bytes()); // timestamp ms
                    frame.extend_from_slice(&(pkt.len() as u32).to_be_bytes());
                    frame.extend_from_slice(&pkt);
                    Message::Binary(frame.into())
                } else {
                    Message::Binary(pkt.into())
                };
                if let Err(e) = s.ws.send(msg) {
                    close_reason = Some(format!("websocket send failed: {e}"));
                    break;
                }
            }
        } else {
            for _ in encoded_rx.try_iter() {} // drop stale audio
        }

        // one stat line per second while listening
        if shared.mic_enabled() && last_stat.elapsed() > Duration::from_secs(1) {
            last_stat = Instant::now();
            log::info!(
                "listening: {up_frames} frames sent, mic rms {:.4}",
                shared.mic_level()
            );
        }

        // after `tts stop`: wait for the tail of the reply to play out, then
        // go idle but KEEP the session — later turns reuse it until the app
        // quits or the server drops it (next press reconnects transparently)
        let backlog_ms = audio.playback_backlog() * 1000 / out_rate as usize;
        if draining
            && shared.phase() == Phase::Speaking
            && jitter.is_empty()
            && backlog_ms < 50
        {
            draining = false;
            shared.set_phase(Phase::Idle);
            log::info!("reply finished (session kept open)");
        }

        // keepalive ping so idle servers/NAT don't drop the session
        if last_ping.elapsed() > Duration::from_secs(25) {
            last_ping = Instant::now();
            if let Err(e) = s.ws.send(Message::Ping(Bytes::new())) {
                close_reason = Some(format!("ping failed: {e}"));
            }
        }

            inbound_stale = s.last_inbound.elapsed() > INBOUND_TIMEOUT;
        }

        if let Some(reason) = close_reason {
            log::info!("closing session: {reason}");
            close_session(&mut session, &shared);
            draining = false;
        } else if inbound_stale {
            log::warn!("no inbound traffic for 120s, closing");
            close_session(&mut session, &shared);
        }
    }
}

/// Command pressed: start/interrupt/resume a conversation turn.
fn on_cmd_down(
    session: &mut Option<Session>,
    draining: &mut bool,
    config: &Connection,
    shared: &Arc<SharedState>,
    decode_rate: &Arc<AtomicU32>,
    audio: &AudioHandles,
) {
    match shared.phase() {
        Phase::Listening => {}
        // session already open (kept from the last turn) — just start listening
        Phase::Speaking | Phase::Waiting | Phase::Idle if session.is_some() => {
            let s = session.as_mut().unwrap();
            if shared.phase() == Phase::Speaking {
                // interrupt the current reply, drop buffered audio too
                if let Err(e) = send_json(s, json_obj("abort")) {
                    log::warn!("abort failed: {e:#}");
                }
                audio.jitter.clear();
                audio.flush_playback();
            }
            *draining = false;
            match send_json(s, json_obj("listen").state("start").mode("manual")) {
                Ok(()) => {
                    shared.set_phase(Phase::Listening);
                    shared.set_mic_enabled(true);
                }
                Err(e) => {
                    // stale socket: drop it and reconnect from scratch
                    log::warn!("listen start failed: {e:#}; reconnecting");
                    close_session(session, shared);
                    start_new_session(session, draining, config, shared, decode_rate);
                }
            }
        }
        _ => start_new_session(session, draining, config, shared, decode_rate),
    }
}

/// Open a fresh session and enter listening.
fn start_new_session(
    session: &mut Option<Session>,
    draining: &mut bool,
    config: &Connection,
    shared: &Arc<SharedState>,
    decode_rate: &Arc<AtomicU32>,
) {
    shared.set_phase(Phase::Connecting);
    match open_session(config, decode_rate) {
        Ok(mut s) => {
            match send_json(&mut s, json_obj("listen").state("start").mode("manual")) {
                Ok(()) => {
                    shared.set_phase(Phase::Listening);
                    shared.set_mic_enabled(true);
                    *draining = false;
                    *session = Some(s);
                }
                Err(e) => {
                    log::warn!("listen start failed: {e:#}");
                    close_session(session, shared);
                }
            }
        }
        Err(e) => {
            log::warn!("could not open audio channel: {e:#}");
            shared.set_phase(Phase::Idle);
        }
    }
}

/// Command released: stop listening, wait for the reply.
fn on_cmd_up(session: &mut Option<Session>, shared: &Arc<SharedState>) {
    if shared.phase() != Phase::Listening {
        return;
    }
    if let Some(s) = session.as_mut() {
        shared.set_mic_enabled(false);
        if let Err(e) = send_json(s, json_obj("listen").state("stop")) {
            log::warn!("listen stop failed: {e:#}");
        }
        shared.set_phase(Phase::Waiting);
    }
}

/// Connect and exchange hellos.
fn open_session(config: &Connection, decode_rate: &Arc<AtomicU32>) -> Result<Session> {
    let deadline = Instant::now() + HELLO_TIMEOUT;
    log::info!("connecting to {}", config.url);

    let mut request = config.url.as_str().into_client_request()?;
    let headers = request.headers_mut();
    headers.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", config.token))?,
    );
    headers.insert("Protocol-Version", HeaderValue::from(PROTOCOL_VERSION));
    headers.insert("Device-Id", HeaderValue::from_str(&config.device_id)?);
    headers.insert("Client-Id", HeaderValue::from_str(&config.client_id)?);

    let (mut ws, _) = tungstenite::client::connect(request)
        .map_err(|e| anyhow::anyhow!("websocket connect failed: {e}"))?;

    // hello: version 2 carries per-frame timestamps so the SERVER can run
    // echo cancellation; plain v1 sends bare opus frames.
    let hello = serde_json::json!({
        "type": "hello",
        "version": if config.server_aec { 2 } else { PROTOCOL_VERSION },
        "features": { "mcp": true, "aec": config.server_aec },
        "transport": "websocket",
        "audio_params": {
            "format": "opus",
            "sample_rate": crate::audio::MIC_RATE,
            "channels": 1,
            "frame_duration": crate::audio::FRAME_MS,
        }
    });
    ws.send(Message::text(hello.to_string()))?;

    // wait for the server hello
    let resp = loop {
        let Some(msg) = read_until(&mut ws, deadline)? else {
            bail!("server hello timeout");
        };
        match msg {
            Message::Text(t) => {
                let v: serde_json::Value =
                    serde_json::from_str(t.as_str()).context("server hello is not JSON")?;
                if v.get("type").and_then(|t| t.as_str()) == Some("hello") {
                    break v;
                }
                log::debug!("pre-hello message: {t}");
            }
            Message::Binary(_) => bail!("binary frame before server hello"),
            _ => {}
        }
    };

    if resp.get("transport").and_then(|t| t.as_str()) != Some("websocket") {
        bail!("server hello transport is not websocket");
    }
    let session_id = resp
        .get("session_id")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();

    if let Some(params) = resp.get("audio_params") {
        if let Some(rate) = params.get("sample_rate").and_then(|r| r.as_u64()) {
            decode_rate.store(rate as u32, Ordering::Relaxed);
            log::info!("server audio: {rate} Hz opus");
        }
    }
    log::info!(
        "session {} established",
        if session_id.is_empty() { "<none>" } else { &session_id }
    );

    let version = if config.server_aec { 2 } else { PROTOCOL_VERSION };

    // 50 ms read timeout turns ws.read() into a poll loop
    match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(READ_POLL))?,
        MaybeTlsStream::NativeTls(t) => t.get_ref().set_read_timeout(Some(READ_POLL))?,
        other => bail!("unexpected stream variant {other:?}"),
    }

    Ok(Session {
        ws,
        session_id,
        last_inbound: Instant::now(),
        version,
        frames_sent: 0,
    })
}

/// Blocking read that gives up at `deadline` (None = timed out).
fn read_until(ws: &mut Ws, deadline: Instant) -> Result<Option<Message>> {
    loop {
        match ws.read() {
            Ok(m) => return Ok(Some(m)),
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn close_session(session: &mut Option<Session>, shared: &Arc<SharedState>) {
    shared.set_mic_enabled(false);
    shared.set_level(0.0);
    shared.set_emotion(0);
    shared.set_phase(Phase::Idle);
    if let Some(mut s) = session.take() {
        let _ = s.ws.send(Message::Close(None));
        let _ = s.ws.flush();
    }
}

// ---------------------------------------------------------------------------
// JSON messages
// ---------------------------------------------------------------------------

/// Small builder so message construction reads like the protocol doc.
fn json_obj(kind: &str) -> MsgBuilder {
    MsgBuilder {
        v: serde_json::json!({ "type": kind }),
    }
}

struct MsgBuilder {
    v: serde_json::Value,
}

impl From<MsgBuilder> for serde_json::Value {
    fn from(m: MsgBuilder) -> Self {
        m.v
    }
}

impl MsgBuilder {
    fn state(mut self, s: &str) -> Self {
        self.v["state"] = s.into();
        self
    }
    fn mode(mut self, m: &str) -> Self {
        self.v["mode"] = m.into();
        self
    }
}

/// Attach the session id and send.
fn send_json(s: &mut Session, v: impl Into<serde_json::Value>) -> Result<()> {
    let mut v = v.into();
    if !s.session_id.is_empty() {
        v["session_id"] = s.session_id.clone().into();
    }
    log::debug!("-> {v}");
    s.ws
        .send(Message::text(v.to_string()))
        .context("websocket send failed")
}

/// Handle one server JSON message; returns an optional reply to send.
fn handle_json(
    text: &str,
    session_id: &mut String,
    conn: &Connection,
    shared: &Arc<SharedState>,
    decode_rate: &Arc<AtomicU32>,
    draining: &mut bool,
) -> Result<Option<serde_json::Value>> {
    let v: serde_json::Value = serde_json::from_str(text)?;
    let mut reply = None;
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "tts" => match v.get("state").and_then(|s| s.as_str()) {
            Some("start") => {
                if shared.phase() != Phase::Listening {
                    shared.set_phase(Phase::Speaking);
                }
            }
            Some("stop") => {
                if shared.phase() == Phase::Speaking {
                    *draining = true;
                }
            }
            Some("sentence_start") => {
                let t = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
                log::info!("tts: {t}");
            }
            _ => {}
        },
        "stt" => {
            let t = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
            log::info!("stt: {t}");
        }
        "llm" => {
            let e = v.get("emotion").and_then(|e| e.as_str()).unwrap_or("");
            shared.set_emotion(crate::ui::emotion_index(e));
            log::info!("emotion: {e}");
        }
        "hello" => {
            // a fresh hello mid-connection: adopt the new session id / rate
            if let Some(id) = v.get("session_id").and_then(|s| s.as_str()) {
                *session_id = id.to_string();
            }
            if let Some(rate) = v
                .get("audio_params")
                .and_then(|p| p.get("sample_rate"))
                .and_then(|r| r.as_u64())
            {
                decode_rate.store(rate as u32, Ordering::Relaxed);
            }
        }
        "system" => log::warn!("system message: {v}"),
        "alert" => {
            let m = v.get("message").and_then(|m| m.as_str()).unwrap_or("");
            log::warn!("server alert: {m}");
        }
        "mcp" => reply = handle_mcp(conn, shared, &v),
        other => log::debug!("unhandled message type {other}"),
    }
    Ok(reply)
}

/// MCP server surface. Every JSON-RPC *request* (one with an `id`) must be
/// answered with the same `id` — a missing reply stalls the server's tool
/// turn (that is the "volume change hangs" bug). Notifications (no `id`)
/// need no reply.
fn handle_mcp(conn: &Connection, shared: &Arc<SharedState>, v: &serde_json::Value) -> Option<serde_json::Value> {
    let p = v.get("payload")?;
    let method = p.get("method").and_then(|m| m.as_str())?;
    let id = p.get("id").cloned();
    let reply = |payload: serde_json::Value| {
        Some(serde_json::json!({ "type": "mcp", "payload": payload }))
    };
    let tool_text = |text: String| {
        serde_json::json!({
            "content": [ { "type": "text", "text": text } ],
            "isError": false
        })
    };
    let rpc_error = |id: serde_json::Value, code: i64, message: String| {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
    };

    let Some(id) = id else {
        log::debug!("mcp notification {method} ignored");
        return None;
    };

    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "ruozhi", "version": env!("CARGO_PKG_VERSION") }
        }),
        "tools/list" => serde_json::json!({
            "tools": [
                {
                    "name": "self.get_device_status",
                    "description": "Provides the device's current status information, including audio speaker volume and other attributes",
                    "inputSchema": { "type": "object", "properties": {} }
                },
                {
                    "name": "self.audio_speaker.set_volume",
                    "description": "Set the speaker volume, ranging from 0 to 100",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "volume": { "type": "number", "description": "Volume level from 0 to 100" }
                        },
                        "required": ["volume"]
                    }
                },
                {
                    "name": "self.get_device_info",
                    "description": "Provides this machine's hardware and system information: OS version, kernel, CPU model and core counts, total and used memory in GB, and GPU model(s)",
                    "inputSchema": { "type": "object", "properties": {} }
                },
                {
                    "name": "self.screen.set_brightness",
                    "description": "Set the screen brightness, ranging from 0 to 100",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "brightness": { "type": "number", "description": "Brightness level from 0 to 100" }
                        },
                        "required": ["brightness"]
                    }
                },
                {
                    "name": "self.reboot",
                    "description": "Restart the xiaozhi application",
                    "inputSchema": { "type": "object", "properties": {} }
                }
            ]
        }),
        "tools/call" => {
            let name = p
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args = p.pointer("/params/arguments").cloned().unwrap_or_default();
            match name.as_str() {
                "self.audio_speaker.set_volume" => {
                    let vol = args.get("volume").and_then(|v| v.as_f64());
                    match vol {
                        Some(v) if (0.0..=100.0).contains(&v) => {
                            conn.volume.store(v as u32, AtomicOrdering::Relaxed);
                            log::info!("volume set to {}", v as u32);
                            tool_text(format!("{{\"volume\": {}}}", v as u32))
                        }
                        _ => {
                            return reply(rpc_error(
                                id.clone(),
                                -32602,
                                "invalid params: volume must be a number from 0 to 100".into(),
                            ))
                        }
                    }
                }
                "self.get_device_status" => {
                    let vol = conn.volume.load(AtomicOrdering::Relaxed);
                    let mut status = serde_json::json!({
                        "audio_speaker": { "volume": vol },
                        "network": {
                            "online": true,
                            "ip": crate::hostinfo::local_ip(),
                        }
                    });
                    if let Some((level, charging)) = crate::hostinfo::battery() {
                        status["battery"] = serde_json::json!({ "level": level, "charging": charging });
                    }
                    if let Some(b) = crate::hostinfo::get_brightness() {
                        status["screen"] =
                            serde_json::json!({ "brightness": (b * 100.0).round() as u32 });
                    }
                    tool_text(status.to_string())
                }
                "self.screen.set_brightness" => {
                    let b = args.get("brightness").and_then(|b| b.as_f64());
                    match b {
                        Some(b) if (0.0..=100.0).contains(&b) => {
                            if crate::hostinfo::set_brightness((b as f32 / 100.0).clamp(0.05, 1.0)) {
                                log::info!("brightness set to {}", b as u32);
                                tool_text(format!("{{\"brightness\": {}}}", b as u32))
                            } else {
                                return reply(rpc_error(
                                    id.clone(),
                                    -32000,
                                    "brightness control not available on this display".into(),
                                ))
                            }
                        }
                        _ => {
                            return reply(rpc_error(
                                id.clone(),
                                -32602,
                                "invalid params: brightness must be a number from 0 to 100".into(),
                            ))
                        }
                    }
                }
                "self.reboot" => {
                    shared.set_action(crate::state::ACTION_RESTART);
                    tool_text("\"rebooting\"".to_string())
                }
                "self.get_device_info" => {
                    tool_text(crate::hostinfo::json().to_string())
                }
                other => {
                    return reply(rpc_error(id.clone(), -32602, format!("Unknown tool: {other}")))
                }
            }
        }
        other => {
            // unknown request: must still answer so the server never stalls
            log::debug!("mcp request {other} -> error reply");
            return reply(rpc_error(id.clone(), -32601, format!("Method not found: {other}")));
        }
    };

    if method == "tools/list" {
        let names: Vec<&str> = result["tools"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t["name"].as_str()).collect())
            .unwrap_or_default();
        log::info!("mcp: answered tools/list [{}]", names.join(", "));
    } else {
        log::info!("mcp: answered {method}");
    }
    reply(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}
