//! Audio I/O.
//!
//! Capture: cpal input stream (device rate, N ch) -> mono downmix -> ring ->
//! worker: resample to 16 kHz -> 60 ms opus frames -> `encoded_rx` for the
//! network thread.
//!
//! Playback: network thread pushes opus packets into [`JitterBuf`] -> worker:
//! decode (server sample rate) -> resample to device rate -> ring -> cpal
//! output stream.

use crate::state::SharedState;
use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, Receiver, Sender};
use opus::{Application, Bitrate, Channels, Decoder, Encoder};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const MIC_RATE: u32 = 16_000;

/// Where encoded audio comes from: the real microphone, or a 16-bit PCM WAV
/// file injected at real-time pace (`--wav`, useful when mic access is
/// unavailable and for reproducible end-to-end tests).
#[derive(Clone, Debug)]
pub enum Source {
    Mic,
    Wav(PathBuf),
}
pub const FRAME_MS: usize = 60;
pub const FRAME_SAMPLES: usize = MIC_RATE as usize * FRAME_MS / 1000; // 960
pub const DEFAULT_PLAY_RATE: u32 = 24_000;

// ---------------------------------------------------------------------------
// Shared queue types
// ---------------------------------------------------------------------------

/// User-set speaker volume (0..=100), driven by the MCP tool
/// `self.audio_speaker.set_volume`. Software gain only.
pub type Volume = Arc<AtomicU32>;

pub const DEFAULT_VOLUME: u32 = 100;

/// Queue of encoded opus packets arriving from the network, drained by the
/// playback worker at (roughly) real time. Capped to survive bursty sends.
pub struct JitterBuf {
    q: Mutex<VecDeque<Vec<u8>>>,
    cv: Condvar,
}

impl JitterBuf {
    pub fn new() -> Self {
        Self {
            q: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
        }
    }

    pub fn push(&self, pkt: Vec<u8>) {
        let mut q = self.q.lock().unwrap();
        if q.len() >= 96 {
            q.pop_front();
        }
        q.push_back(pkt);
        drop(q);
        self.cv.notify_all();
    }

    pub fn pop_timeout(&self, timeout: Duration) -> Option<Vec<u8>> {
        let mut q = self.q.lock().unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(p) = q.pop_front() {
                return Some(p);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = self.cv.wait_timeout(q, deadline - now).unwrap();
            q = guard;
        }
    }

    pub fn clear(&self) {
        self.q.lock().unwrap().clear();
    }

    pub fn len(&self) -> usize {
        self.q.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Tiny bounded mono f32 ring between cpal callbacks and worker threads.
/// `level` tracks the buffered sample count (approximate, lock-free reads).
#[derive(Clone)]
struct MonoRing {
    q: Arc<Mutex<VecDeque<f32>>>,
    level: Arc<AtomicIsize>,
    cap: usize,
}

impl MonoRing {
    fn new(cap: usize) -> Self {
        Self {
            q: Arc::new(Mutex::new(VecDeque::with_capacity(cap))),
            level: Arc::new(AtomicIsize::new(0)),
            cap,
        }
    }

    fn level(&self) -> usize {
        self.level.load(Ordering::Relaxed).max(0) as usize
    }

    /// Non-blocking; drops samples when full or contended (callback context).
    fn push(&self, s: f32) {
        if let Ok(mut q) = self.q.try_lock() {
            if q.len() < self.cap {
                q.push_back(s);
                self.level.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn drain(&self, out: &mut Vec<f32>) {
        if let Ok(mut q) = self.q.lock() {
            out.extend(q.drain(..));
        }
    }

    /// Callback: pop one sample, silence when dry.
    fn pop(&self) -> Option<f32> {
        let s = self.q.try_lock().ok().and_then(|mut q| q.pop_front());
        if s.is_some() {
            self.level.fetch_sub(1, Ordering::Relaxed);
        }
        s
    }

    /// Drop everything buffered (barge-in).
    fn clear(&self) {
        if let Ok(mut q) = self.q.lock() {
            self.level.store(0, Ordering::Relaxed);
            q.clear();
        }
    }

    /// Worker: blocking push, waits until everything is in the ring.
    fn push_all_blocking(&self, samples: &[f32]) {
        let mut pushed = 0;
        while pushed < samples.len() {
            if let Ok(mut q) = self.q.try_lock() {
                let free = self.cap - q.len();
                let n = free.min(samples.len() - pushed);
                q.extend(samples[pushed..pushed + n].iter().copied());
                self.level.fetch_add(n as isize, Ordering::Relaxed);
                pushed += n;
            }
            if pushed < samples.len() {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Resampling (mono f32)
// ---------------------------------------------------------------------------

/// Stateful, anti-aliased windowed-sinc resampler for arbitrary rate pairs.
///
/// Downsampling low-pass filters at the target Nyquist (no aliasing), which
/// linear interpolation does not — the old path aliased 8-22 kHz mic energy
/// straight into the speech band and hurt server-side ASR. The kernel is
/// precomputed at 512 sub-phases (blackman-windowed sinc), so per output
/// sample is just a dot product.
pub struct Resampler {
    /// input samples per output sample
    step: f64,
    /// absolute input-domain position of the next output sample
    pos: f64,
    /// absolute input index of buf.front()
    base: i64,
    buf: std::collections::VecDeque<f32>,
    half: usize,
    phases: usize,
    table: Vec<f32>, // phases * (2*half+1)
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let step = from as f64 / to as f64;
        // cutoff relative to input nyquist; slight margin avoids a brick at 8k
        let cutoff = if to < from { 0.45 * to as f64 / from as f64 } else { 0.5 };
        let zero_xings = 16.0;
        let half = ((zero_xings / (2.0 * cutoff)).ceil() as usize).max(4);
        let phases = 512;
        let width = 2 * half + 1;
        let tau = std::f64::consts::PI;

        let mut table = vec![0f32; phases * width];
        for p in 0..phases {
            let frac = p as f64 / phases as f64; // kernel center offset after the nearest sample
            let mut sum = 0f64;
            for t in 0..width {
                let x = t as f64 - half as f64 - frac; // distance from center, in input samples
                let arg = 2.0 * cutoff * x;
                let sinc = if arg.abs() < 1e-9 {
                    1.0
                } else {
                    (tau * arg).sin() / (tau * arg)
                };
                let w = x / (half as f64 + 1.0);
                let win = 0.42 + 0.5 * (tau * w).cos() + 0.08 * (2.0 * tau * w).cos();
                let v = sinc * win;
                table[p * width + t] = v as f32;
                sum += v;
            }
            if sum.abs() > 1e-9 {
                for t in 0..width {
                    table[p * width + t] /= sum as f32; // unity DC gain per phase
                }
            }
        }

        Self {
            step,
            pos: 0.0,
            base: 0,
            buf: std::collections::VecDeque::with_capacity(half * 4 + 1024),
            half,
            phases,
            table,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for &s in input {
            self.buf.push_back(s);
        }
        let width = 2 * self.half + 1;
        // emit while the kernel around pos fits in buffered input
        while (self.pos.floor() as i64 + self.half as i64) < self.base + self.buf.len() as i64 {
            let center = self.pos.floor() as i64;
            let frac = self.pos - center as f64;
            let q = ((frac * self.phases as f64) as usize).min(self.phases - 1);
            let kern = &self.table[q * width..(q + 1) * width];
            let first = center - self.half as i64;
            let mut acc = 0f32;
            for (t, &k) in kern.iter().enumerate() {
                let idx = first + t as i64 - self.base;
                if idx >= 0 && (idx as usize) < self.buf.len() {
                    acc += k * self.buf[idx as usize];
                }
            }
            out.push(acc);
            self.pos += self.step;
        }
        // trim consumed history: keep half a kernel behind pos
        let keep_from = self.pos.floor() as i64 - self.half as i64 - 1;
        let drop = (keep_from - self.base).max(0) as usize;
        for _ in 0..drop.min(self.buf.len()) {
            self.buf.pop_front();
            self.base += 1;
        }
    }
}

/// Uplink automatic gain control: keeps steady-state speech rms near
/// `TARGET` so a quiet mic still reaches the server at a healthy level.
/// Software-only — never touches system input gain. Never attenuates
/// (gain >= 1): a hot mic is left alone. Digital silence stays silent
/// (0 * gain = 0), so the no-permission watchdog still works.
struct MicAgc {
    gain: f32,
    level: f32,
}

impl MicAgc {
    const TARGET: f32 = 0.12;
    const MAX_GAIN: f32 = 12.0;
    const SILENCE: f32 = 0.0008;

    fn new() -> Self {
        Self {
            gain: 1.0,
            level: 0.0,
        }
    }

    fn process(&mut self, chunk: &mut [f32]) {
        // smoothed chunk level (~50 ms)
        self.level += (rms(chunk) - self.level) * 0.12;
        let want = if self.level > Self::SILENCE {
            (Self::TARGET / self.level).clamp(1.0, Self::MAX_GAIN)
        } else {
            self.gain // hold on (digital) silence, don't chase the noise floor
        };
        self.gain += (want - self.gain) * 0.05; // slow, ~100 ms
        for s in chunk.iter_mut() {
            *s = (*s * self.gain).clamp(-0.98, 0.98);
        }
        // fast backoff if we are clipping
        if chunk.iter().any(|s| s.abs() > 0.95) {
            self.gain = (self.gain * 0.9).max(1.0);
        }
    }
}

/// Downlink auto volume: eases loud TTS replies toward a comfortable level.
/// Software-only attenuation (gain <= 1): quiet replies are never boosted.
/// The user-set [`Volume`] (MCP tool) applies on top, multiplicatively.
struct PlayAutoVolume {
    gain: f32,
    level: f32,
    volume: Volume,
}

impl PlayAutoVolume {
    const TARGET: f32 = 0.09;
    const MIN_GAIN: f32 = 0.15;

    fn new(volume: Volume) -> Self {
        Self {
            gain: 1.0,
            level: 0.0,
            volume,
        }
    }

    fn process(&mut self, chunk: &mut [f32]) {
        self.level += (rms(chunk) - self.level) * 0.1;
        if self.level > Self::TARGET && self.level > 0.001 {
            let want = (Self::TARGET / self.level).clamp(Self::MIN_GAIN, 1.0);
            self.gain += (want - self.gain) * 0.08;
        } else {
            self.gain += (1.0 - self.gain) * 0.02; // drift back up when quiet
        }
        let user = self.volume.load(Ordering::Relaxed).min(100) as f32 / 100.0;
        for s in chunk.iter_mut() {
            *s *= self.gain * user;
        }
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

pub struct AudioHandles {
    /// Encoded 60 ms opus frames from the mic (16 kHz mono).
    pub encoded_rx: Receiver<Vec<u8>>,
    /// Encoded opus packets to play, fed by the network thread.
    pub jitter: Arc<JitterBuf>,
    /// Server sample rate (from the hello audio_params); playback adapts.
    pub decode_rate: Arc<AtomicU32>,
    play_ring: MonoRing,
    /// Output device sample rate, for turning backlog into milliseconds.
    pub out_rate: u32,
    _streams: Vec<cpal::Stream>,
}

/// Per-platform heads-up about where the mic permission lives, logged once
/// at startup (only for the real-mic source).
#[cfg(target_os = "macos")]
fn log_mic_permission_hint() {
    // macOS attributes mic permission to the *responsible* process:
    // `open ...app` -> this bundle; direct binary launch -> the hosting
    // terminal. __CFBundleIdentifier tells them apart.
    match std::env::var("__CFBundleIdentifier") {
        Ok(id) if id == "local.ruozhi.app" => {
            log::debug!("launched as the app bundle; mic permission belongs to ruozhi")
        }
        Ok(id) => log::info!(
            "launched from inside app `{id}` — macOS checks ITS microphone permission; \
             if audio ends up silent, launch via `open target/release/ruozhi.app` \
             (logs: RUOZHI_LOG_FILE) or allow `{id}` in microphone settings"
        ),
        Err(_) => log::info!(
            "launched as a bare binary — macOS checks the hosting terminal's microphone \
             permission; if audio ends up silent, launch via `open target/release/ruozhi.app` \
             (logs: RUOZHI_LOG_FILE) or allow the terminal in microphone settings"
        ),
    }
}

#[cfg(windows)]
fn log_mic_permission_hint() {
    // Windows gates desktop apps as a group — there is no per-bundle identity.
    log::info!(
        "windows mic privacy is global for desktop apps: if audio ends up silent, \
         allow desktop apps under 设置 → 隐私和安全性 → 麦克风 (“允许桌面应用访问麦克风”)"
    );
}

#[cfg(all(unix, not(target_os = "macos")))]
fn log_mic_permission_hint() {
    log::debug!("no platform mic-permission gate to hint about");
}

/// Fix hint logged by the silence watchdog, per platform.
fn silent_input_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "fix: `open target/release/ruozhi.app` (bundle has its own mic permission), \
         or allow this app's host terminal under 系统设置 → 隐私与安全性 → 麦克风 and restart it"
    }
    #[cfg(windows)]
    {
        "fix: check the mic device selection/mute, and allow desktop apps under \
         设置 → 隐私和安全性 → 麦克风"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "fix: check the default input device and your PipeWire/PulseAudio permissions"
    }
}

impl AudioHandles {
    /// Drop anything queued for playback (barge-in on abort).
    pub fn flush_playback(&self) {
        self.play_ring.clear();
    }

    /// Samples still queued for playback.
    pub fn playback_backlog(&self) -> usize {
        self.play_ring.level()
    }
}

pub fn spawn(shared: Arc<SharedState>, source: Source, volume: Volume) -> Result<AudioHandles> {
    let (encoded_tx, encoded_rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = bounded(64);
    let jitter = Arc::new(JitterBuf::new());
    let decode_rate = Arc::new(AtomicU32::new(DEFAULT_PLAY_RATE));
    let mut streams = Vec::new();

    // ----- capture -----
    let (in_rate, cap_ring) = match &source {
        Source::Mic => {
            let in_dev = cpal::default_host().default_input_device().context("no default input device")?;
            match in_dev.description() {
                Ok(desc) => log::info!("input device: {} ({:?})", desc.name(), desc.device_type()),
                Err(e) => log::debug!("input device description unavailable: {e}"),
            }
            // Where does mic permission live on this OS? Silent capture
            // failure is the #1 "it only says 嗯" bug; each OS attributes
            // the permission differently (see below).
            log_mic_permission_hint();
            let in_cfg = in_dev
                .default_input_config()
                .context("no default input config")?;
            let in_rate = in_cfg.sample_rate();
            let in_ch = in_cfg.channels() as usize;
            log::info!("input: {} Hz x {} ch ({:?})", in_rate, in_ch, in_cfg.sample_format());

            let ring = MonoRing::new(in_rate as usize); // ~1 s
            let stream = match in_cfg.sample_format() {
                cpal::SampleFormat::F32 => {
                    let r = ring.clone();
                    let err = move |e| log::warn!("input stream error: {e}");
                    in_dev.build_input_stream(
                        in_cfg.clone().into(),
                        move |d: &[f32], _| {
                            for frame in d.chunks(in_ch) {
                                r.push(frame.iter().sum::<f32>() / in_ch as f32);
                            }
                        },
                        err,
                        None,
                    )?
                }
                cpal::SampleFormat::I16 => {
                    let r = ring.clone();
                    let err = move |e| log::warn!("input stream error: {e}");
                    in_dev.build_input_stream(
                        in_cfg.clone().into(),
                        move |d: &[i16], _| {
                            for frame in d.chunks(in_ch) {
                                let s = frame.iter().map(|&v| v as i32).sum::<i32>() as f32
                                    / in_ch as f32
                                    / 32768.0;
                                r.push(s);
                            }
                        },
                        err,
                        None,
                    )?
                }
                sf => bail!("unsupported input sample format: {sf:?}"),
            };
            stream.play()?;
            streams.push(stream);
            (in_rate, ring)
        }
        Source::Wav(path) => {
            let (samples, rate) = parse_wav(path)?;
            log::info!("input: wav file {} ({} Hz, {} samples)", path.display(), rate, samples.len());
            let ring = MonoRing::new(rate as usize * 2);
            let r = ring.clone();
            let wav_gate = shared.clone();
            // real-time pacer: waits for mic_enabled (the talk button), then
            // streams the file into the capture ring at native rate
            std::thread::Builder::new()
                .name("wav-source".into())
                .spawn(move || {
                    let chunk = (rate as usize / 50).max(1); // 20 ms
                    let mut pos = 0;
                    loop {
                        if !wav_gate.mic_enabled() {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        let end = (pos + chunk).min(samples.len());
                        for &v in &samples[pos..end] {
                            r.push(v as f32 / 32768.0);
                        }
                        pos = end;
                        if pos >= samples.len() {
                            // file finished: keep feeding silence like an idle mic
                            for _ in 0..chunk {
                                r.push(0.0);
                            }
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                })?;
            (rate, ring)
        }
    };

    // capture worker: ring -> 16 kHz -> opus frames
    {
        let shared = shared.clone();
        let encoded_tx = encoded_tx.clone();
        std::thread::Builder::new()
            .name("mic-encode".into())
            .spawn(move || {
                let mut enc = match Encoder::new(MIC_RATE, Channels::Mono, Application::Audio) {
                    Ok(e) => e,
                    Err(e) => {
                        log::error!("opus encoder init failed: {e}");
                        return;
                    }
                };
                let _ = enc.set_bitrate(Bitrate::Bits(32_000));
                let _ = enc.set_vbr(true);
                let _ = enc.set_complexity(8);
                let _ = enc.set_dtx(false);

                let mut rs = Resampler::new(in_rate, MIC_RATE);
                let mut agc = MicAgc::new();
                let mut raw = Vec::new();
                let mut resampled = Vec::new();
                let mut accum: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES * 2);
                let mut pkt = [0u8; 4000];
                // silence watchdog: if a whole talk turn (~3 s wall clock)
                // never rises above noise, the device has no signal
                let mut turn_start: Option<Instant> = None;
                let mut turn_max: f32 = 0.0;
                let mut silence_warned = false;
                loop {
                    std::thread::sleep(Duration::from_millis(5));
                    raw.clear();
                    cap_ring.drain(&mut raw);
                    if raw.is_empty() {
                        if !shared.mic_enabled() {
                            accum.clear();
                        }
                        continue;
                    }
                    resampled.clear();
                    rs.process(&raw, &mut resampled);
                    agc.process(&mut resampled);
                    let level = rms(&resampled);
                    shared.set_mic_level(level);
                    if shared.mic_enabled() {
                        let ts = *turn_start.get_or_insert_with(Instant::now);
                        turn_max = turn_max.max(level);
                        if ts.elapsed() >= Duration::from_secs(3)
                            && turn_max < 0.002
                            && !silence_warned
                        {
                            silence_warned = true;
                            log::warn!(
                                "talk button held for 3s but input max level is {:.5} — no signal. {}",
                                turn_max,
                                silent_input_hint()
                            );
                        }
                    } else {
                        turn_start = None;
                        turn_max = 0.0;
                        silence_warned = false;
                    }
                    for &s in &resampled {
                        accum.push((s.clamp(-1.0, 1.0) * 32767.0) as i16);
                    }
                    while accum.len() >= FRAME_SAMPLES {
                        let frame: Vec<i16> = accum.drain(..FRAME_SAMPLES).collect();
                        if !shared.mic_enabled() {
                            continue;
                        }
                        match enc.encode(&frame, &mut pkt) {
                            Ok(n) => {
                                if encoded_tx.try_send(pkt[..n].to_vec()).is_err() {
                                    log::debug!("encoded channel full, dropping frame");
                                }
                            }
                            Err(e) => log::warn!("opus encode failed: {e}"),
                        }
                    }
                    if !shared.mic_enabled() {
                        accum.clear();
                    }
                }
            })?;
    }

    // ----- playback -----
    let out_dev = cpal::default_host().default_output_device().context("no default output device")?;
    let out_cfg = out_dev
        .default_output_config()
        .context("no default output config")?;
    let out_rate = out_cfg.sample_rate();
    let out_ch = out_cfg.channels() as usize;
    log::info!("output: {} Hz x {} ch ({:?})", out_rate, out_ch, out_cfg.sample_format());

    let play_ring = MonoRing::new(out_rate as usize * 2); // ~2 s
    {
        let ring = play_ring.clone();
        let err = move |e| log::warn!("output stream error: {e}");
        let stream = match out_cfg.sample_format() {
            cpal::SampleFormat::F32 => out_dev.build_output_stream(
                out_cfg.clone().into(),
                move |d: &mut [f32], _| {
                    for chunk in d.chunks_mut(out_ch) {
                        let s = ring.pop().unwrap_or(0.0);
                        for o in chunk {
                            *o = s;
                        }
                    }
                },
                err,
                None,
            )?,
            sf => bail!("unsupported output sample format: {sf:?}"),
        };
        stream.play()?;
        streams.push(stream);
    }

    // playback worker: jitter -> decode -> resample -> ring
    {
        let shared = shared.clone();
        let decode_rate = decode_rate.clone();
        let ring = play_ring.clone();
        let jitter = jitter.clone();
        std::thread::Builder::new()
            .name("tts-play".into())
            .spawn(move || {
                let mut cur_rate = decode_rate.load(Ordering::Relaxed);
                let mut decoder = match Decoder::new(cur_rate, Channels::Mono) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        log::error!("opus decoder init failed: {e}");
                        None
                    }
                };
                let mut rs = Resampler::new(cur_rate, out_rate);
                let mut auto_vol = PlayAutoVolume::new(volume.clone());
                let mut pcm = vec![0i16; MIC_RATE as usize]; // 60 ms @ 16k is 960; room for 48k
                loop {
                    let pkt = jitter.pop_timeout(Duration::from_millis(700));
                    let Some(pkt) = pkt else { continue };
                    // short prefill to smooth packet bursts
                    let start = Instant::now();
                    while jitter.len() < 2 && start.elapsed() < Duration::from_millis(120) {
                        std::thread::sleep(Duration::from_millis(10));
                    }

                    let rate = decode_rate.load(Ordering::Relaxed);
                    if rate != cur_rate {
                        match Decoder::new(rate, Channels::Mono) {
                            Ok(d) => {
                                decoder = Some(d);
                                rs = Resampler::new(rate, out_rate);
                                cur_rate = rate;
                            }
                            Err(e) => {
                                log::warn!("opus re-decode init at {rate} Hz failed: {e}");
                                continue;
                            }
                        }
                    }
                    let Some(dec) = decoder.as_mut() else { continue };

                    let n = match dec.decode(&pkt, &mut pcm, false) {
                        Ok(n) => n,
                        Err(e) => {
                            log::warn!("opus decode failed: {e}");
                            continue;
                        }
                    };
                    let mut mono: Vec<f32> = pcm[..n].iter().map(|&s| s as f32 / 32768.0).collect();
                    auto_vol.process(&mut mono);
                    shared.set_level(rms(&mono));
                    let mut res = Vec::with_capacity(mono.len() * out_rate as usize / rate as usize + 16);
                    rs.process(&mono, &mut res);
                    ring.push_all_blocking(&res);
                }
            })?;
    }

    Ok(AudioHandles {
        encoded_rx,
        jitter,
        decode_rate,
        play_ring: play_ring.clone(),
        out_rate,
        _streams: streams,
    })
}

// ---------------------------------------------------------------------------
// WAV parsing (16-bit PCM)
// ---------------------------------------------------------------------------

/// Parse a RIFF/WAVE file with 16-bit PCM samples; returns mono samples + rate.
fn parse_wav(path: &std::path::Path) -> Result<(Vec<i16>, u32)> {
    use anyhow::Context;
    let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if raw.len() < 12 || &raw[0..4] != b"RIFF" || &raw[8..12] != b"WAVE" {
        bail!("not a RIFF/WAVE file");
    }
    let mut fmt: Option<(u16, u16, u32)> = None; // (format, channels, rate)
    let mut bits = 0u16;
    let mut data: Option<&[u8]> = None;
    let mut pos = 12;
    while pos + 8 <= raw.len() {
        let tag = &raw[pos..pos + 4];
        let len = u32::from_le_bytes(raw[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = raw.get(pos + 8..pos + 8 + len).context("truncated wav chunk")?;
        match tag {
            b"fmt " => {
                let audio_format = u16::from_le_bytes(body[0..2].try_into().unwrap());
                let channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
                let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
                fmt = Some((audio_format, channels, rate));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        pos += 8 + len + (len & 1); // chunks are word-aligned
    }
    let (format, channels, rate) = fmt.context("wav has no fmt chunk")?;
    if !(format == 1 || format == 0xFFFE) || bits != 16 {
        bail!("only 16-bit PCM wav is supported (format {format}, {bits} bits)");
    }
    let data = data.context("wav has no data chunk")?;
    let mut mono: Vec<i16> = Vec::with_capacity(data.len() / 2 / channels as usize);
    for frame in data.chunks_exact(2 * channels as usize) {
        let sum: i32 = (0..channels as usize)
            .map(|c| i16::from_le_bytes(frame[c * 2..c * 2 + 2].try_into().unwrap()) as i32)
            .sum();
        mono.push((sum / channels as i32) as i16);
    }
    Ok((mono, rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, rate: u32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
    }

    fn resample_all(from: u32, to: u32, input: &[f32]) -> Vec<f32> {
        let mut rs = Resampler::new(from, to);
        let mut out = Vec::new();
        // feed in realistic callback-sized chunks
        for chunk in input.chunks(512) {
            rs.process(chunk, &mut out);
        }
        out
    }

    #[test]
    fn aliasing_tone_is_attenuated() {
        // 12 kHz at 44.1 kHz sits above the 16 kHz output nyquist (8 kHz);
        // it must fold to near-silence, not bleed into the speech band.
        let input = tone(12_000.0, 44_100, 0.5);
        let out = resample_all(44_100, 16_000, &input);
        let out_rms = rms(&out[1000..]); // skip kernel warm-up
        assert!(
            out_rms < 0.02,
            "aliased 12 kHz tone too loud: rms {out_rms}"
        );
    }

    #[test]
    fn passband_tone_is_preserved() {
        let input = tone(1_000.0, 44_100, 0.5);
        let out = resample_all(44_100, 16_000, &input);
        let mid = &out[1000..out.len() - 1000];
        let in_rms = rms(&input);
        let out_rms = rms(mid);
        assert!(
            (out_rms - in_rms).abs() / in_rms < 0.1,
            "1 kHz gain off: in {in_rms} out {out_rms}"
        );
        // frequency check via zero crossings
        let crossings = mid.windows(2).filter(|w| w[0] * w[1] < 0.0).count();
        let dur = mid.len() as f32 / 16_000.0;
        let freq = crossings as f32 / 2.0 / dur;
        assert!((freq - 1000.0).abs() < 20.0, "frequency drifted: {freq} Hz");
    }

    #[test]
    fn mic_agc_boosts_quiet_speech() {
        // quiet mic (rms ~0.02) should be lifted toward the target
        let input = tone(440.0, 16_000, 2.0) // 440 Hz sine, peak 0.5 -> rms 0.354
            .iter()
            .map(|x| x * 0.057) // scale to rms ~0.02
            .collect::<Vec<_>>();
        let mut agc = MicAgc::new();
        let mut out = Vec::new();
        for chunk in input.chunks(80) {
            // ~5 ms ticks like the capture worker
            let mut c = chunk.to_vec();
            agc.process(&mut c);
            out.extend(c);
        }
        let tail = rms(&out[out.len() - 8000..]); // last 0.5 s
        assert!(
            (0.07..0.25).contains(&tail),
            "agc did not lift quiet mic: rms {tail}"
        );
        assert!(
            out.iter().all(|s| s.abs() <= 0.99),
            "agc output clipped"
        );
    }

    #[test]
    fn mic_agc_never_attenuates_hot_mic() {
        let input = tone(440.0, 16_000, 1.0); // rms 0.354, above target
        let mut agc = MicAgc::new();
        let mut out = Vec::new();
        for chunk in input.chunks(80) {
            let mut c = chunk.to_vec();
            agc.process(&mut c);
            out.extend(c);
        }
        let tail = rms(&out[8000..]);
        assert!(tail > 0.25, "agc attenuated a hot mic: rms {tail}");
    }

    #[test]
    fn play_volume_tames_loud_reply() {
        let input = tone(440.0, 24_000, 2.0); // rms 0.354 — a loud reply
        let mut vol = PlayAutoVolume::new(Arc::new(AtomicU32::new(DEFAULT_VOLUME)));
        let mut out = Vec::new();
        for chunk in input.chunks(1440) {
            // ~60 ms packets
            let mut c = chunk.to_vec();
            vol.process(&mut c);
            out.extend(c);
        }
        let tail = rms(&out[out.len() - 12_000..]);
        assert!(
            (0.05..0.14).contains(&tail),
            "auto volume did not tame loud reply: rms {tail}"
        );
    }

    #[test]
    fn play_volume_never_boosts_quiet_reply() {
        let input = tone(440.0, 24_000, 1.0).iter().map(|x| x * 0.06).collect::<Vec<_>>();
        let mut vol = PlayAutoVolume::new(Arc::new(AtomicU32::new(DEFAULT_VOLUME)));
        let mut out = Vec::new();
        for chunk in input.chunks(1440) {
            let mut c = chunk.to_vec();
            vol.process(&mut c);
            out.extend(c);
        }
        let in_rms = rms(&input[8000..]);
        let tail = rms(&out[8000..]);
        assert!(
            (tail - in_rms).abs() / in_rms < 0.15,
            "quiet reply was boosted: rms {tail} vs input {in_rms}"
        );
    }

    #[test]
    fn upsampling_preserves_tone() {
        // playback path: 24 kHz -> 44.1 kHz
        let input = tone(1_000.0, 24_000, 0.5);
        let out = resample_all(24_000, 44_100, &input);
        let mid = &out[1000..out.len() - 1000];
        let out_rms = rms(mid);
        let expect = 0.5 / std::f32::consts::SQRT_2; // rms of a 0.5-amplitude sine
        assert!(
            (out_rms - expect).abs() / expect < 0.1,
            "24k->44.1k gain off: {out_rms}"
        );
    }
}
