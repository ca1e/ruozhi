//! Frame renderers for the 240x240 window. Pixel format is fenster's
//! `0x00RRGGBB`.
//!
//! The visual is one continuous sci-fi "energy core": a solid gradient orb
//! (never hollow) with an outer bloom, internal plasma shimmer, expanding
//! sound-pulse rings while listening/speaking, and comet arcs orbiting it
//! while waiting. Every parameter eases toward its per-phase target so state
//! changes morph instead of cut.

use crate::state::Phase;
use std::sync::OnceLock;

pub const W: usize = 240;
pub const H: usize = 240;

const BG: u32 = 0x08080d;

// ---------------------------------------------------------------------------
// Precomputed tables
// ---------------------------------------------------------------------------

struct Geo {
    /// normalized angle per pixel, 0..1 (one full turn)
    a: Vec<f32>,
    /// distance from center in pixels
    d: Vec<f32>,
    /// x distance from center (for the offset core highlight)
    dx: Vec<f32>,
    dy: Vec<f32>,
}

fn geo() -> &'static Geo {
    static GEO: OnceLock<Geo> = OnceLock::new();
    GEO.get_or_init(|| {
        let (mut a, mut d, mut dx, mut dy) = (
            vec![0f32; W * H],
            vec![0f32; W * H],
            vec![0f32; W * H],
            vec![0f32; W * H],
        );
        let (cx, cy) = ((W as f32 - 1.0) / 2.0, (H as f32 - 1.0) / 2.0);
        for y in 0..H {
            for x in 0..W {
                let i = y * W + x;
                let (px, py) = (x as f32 - cx, y as f32 - cy);
                dx[i] = px;
                dy[i] = py;
                d[i] = (px * px + py * py).sqrt();
                a[i] = (py.atan2(px) / std::f32::consts::TAU + 1.0) % 1.0;
            }
        }
        Geo { a, d, dx, dy }
    })
}

/// Energy gradient, symmetric for seamless angular wrap:
/// cyan -> blue -> violet -> magenta -> pink -> mirrored back.
const STOPS: [(u8, u8, u8); 8] = [
    (0x36, 0xc2, 0xff),
    (0x2e, 0x6b, 0xff),
    (0x6b, 0x4d, 0xff),
    (0xa8, 0x4d, 0xff),
    (0xf7, 0x59, 0xc7),
    (0xa8, 0x4d, 0xff),
    (0x6b, 0x4d, 0xff),
    (0x2e, 0x6b, 0xff),
];

fn palette() -> &'static Vec<u32> {
    static P: OnceLock<Vec<u32>> = OnceLock::new();
    P.get_or_init(|| {
        let n = STOPS.len();
        (0..256)
            .map(|i| {
                let f = i as f32 / 256.0 * n as f32;
                let s = f.floor() as usize % n;
                let t = f.fract();
                let mix = |k: usize| {
                    let ch = |st: (u8, u8, u8)| match k {
                        0 => st.0,
                        1 => st.1,
                        _ => st.2,
                    } as f32;
                    let (c0, c1) = (ch(STOPS[s]), ch(STOPS[(s + 1) % n]));
                    (c0 + (c1 - c0) * t) as u32
                };
                (mix(0) << 16) | (mix(1) << 8) | mix(2)
            })
            .collect()
    })
}

/// sin(2*pi*x) via a 2048-entry lookup; x is in turns.
fn sin2pi(x: f32) -> f32 {
    static S: OnceLock<Vec<f32>> = OnceLock::new();
    let table = S.get_or_init(|| {
        let tau = 2.0 * std::f32::consts::PI;
        (0..2048).map(|i| (i as f32 / 2048.0 * tau).sin()).collect()
    });
    let frac = x - x.floor();
    table[(frac * 2048.0) as usize & 2047]
}

// ---------------------------------------------------------------------------
// Color helpers (0x00RRGGBB)
// ---------------------------------------------------------------------------

fn channel(c: u32, sh: u32) -> f32 {
    ((c >> sh) & 0xff) as f32
}

fn pack(r: f32, g: f32, b: f32) -> u32 {
    (r.clamp(0.0, 255.0) as u32) << 16
        | (g.clamp(0.0, 255.0) as u32) << 8
        | b.clamp(0.0, 255.0) as u32
}

fn ease(cur: f32, target: f32, dt_ms: f32, tau_ms: f32) -> f32 {
    cur + (target - cur) * (1.0 - (-dt_ms / tau_ms).exp())
}

// ---------------------------------------------------------------------------
// The energy core
// ---------------------------------------------------------------------------

/// An expanding sound-pulse ring (birth radius, current radius, alpha).
struct Pulse {
    r: f32,
    alpha: f32,
}

/// Server emotion -> (tint RGB, energy multiplier). Index 0 = neutral/none.
/// Names follow the device firmware's emoji set.
pub const EMOTIONS: [(&str, (u8, u8, u8), f32); 15] = [
    ("neutral", (0x8a, 0x7c, 0xff), 1.00),
    ("happy", (0xff, 0xb3, 0x47), 1.25),
    ("laughing", (0xff, 0xd2, 0x6e), 1.45),
    ("sad", (0x2b, 0x5c, 0xff), 0.65),
    ("crying", (0x3a, 0x74, 0xe8), 0.70),
    ("angry", (0xff, 0x45, 0x45), 1.60),
    ("surprised", (0x36, 0xe0, 0xff), 1.80),
    ("shocked", (0x7d, 0xf5, 0xff), 1.90),
    ("thinking", (0x9d, 0x6b, 0xff), 0.50),
    ("confused", (0xb0, 0x8a, 0xff), 0.75),
    ("loving", (0xff, 0x6e, 0xb5), 1.15),
    ("kissy", (0xff, 0x59, 0xc7), 1.20),
    ("embarrassed", (0xff, 0x7d, 0x9c), 1.10),
    ("sleepy", (0x4a, 0x55, 0x8a), 0.40),
    ("cool", (0x36, 0xc2, 0xff), 1.05),
];

/// Map a server emotion string to its EMOTIONS index (0 when unknown).
pub fn emotion_index(name: &str) -> u8 {
    EMOTIONS
        .iter()
        .position(|(n, _, _)| *n == name)
        .unwrap_or(0) as u8
}

pub struct Orb {
    /// core radius (px)
    size: f32,
    /// outer bloom strength
    glow: f32,
    /// orbiting comet arcs presence (0 = none)
    orbit: f32,
    /// internal plasma shimmer amplitude
    shimmer: f32,
    /// comet orbit speed factor
    spin: f32,
    /// gradient rotation phase (turns)
    swirl: f32,
    /// shimmer phase (turns)
    plasma: f32,
    /// comet head angle (turns)
    head: f32,
    /// active expanding pulses
    pulses: Vec<Pulse>,
    last_spawn_ms: i64,
    t_prev: i64,
}

impl Orb {
    pub fn new() -> Self {
        Self {
            size: 16.0,
            glow: 0.5,
            orbit: 0.15,
            shimmer: 0.3,
            spin: 0.0,
            swirl: 0.0,
            plasma: 0.0,
            head: 0.0,
            pulses: Vec::new(),
            last_spawn_ms: 0,
            t_prev: 0,
        }
    }

    /// Advance the animation one frame and render `phase` into `buf`.
    /// `mic` drives the listening orb, `play` the speaking pulse, `emotion`
    /// (index into EMOTIONS) tints and energizes the orb.
    pub fn draw(
        &mut self,
        buf: &mut [u32],
        phase: Phase,
        t_ms: i64,
        mic: f32,
        play: f32,
        emotion: u8,
    ) {
        let dt = if self.t_prev == 0 {
            16.0
        } else {
            (t_ms - self.t_prev).clamp(1, 120) as f32
        };
        self.t_prev = t_ms;
        let t = t_ms as f32;

        // ---- per-phase parameter targets ----
        let (t_size, t_glow, t_orbit, t_shimmer, t_spin, swirl_v, plasma_v, emit) = match phase {
            Phase::Idle => (
                15.0 + 2.2 * sin2pi(t / 1500.0),
                0.55,
                0.22,
                0.30,
                0.12,
                0.012,
                0.05,
                false,
            ),
            Phase::Connecting => (
                16.5 + 2.0 * sin2pi(t / 240.0),
                0.75,
                0.60,
                0.60,
                0.30,
                0.030,
                0.35,
                false,
            ),
            Phase::Listening => (
                78.0 + (mic * 300.0).clamp(0.0, 14.0),
                0.90,
                0.0,
                1.00,
                0.0,
                0.075,
                0.55,
                true,
            ),
            Phase::Waiting => (60.0, 0.65, 1.00, 0.55, 1.00, 0.030, 0.25, false),
            Phase::Speaking => (
                66.0 + (play * 220.0).clamp(0.0, 12.0),
                0.95,
                0.45,
                0.95,
                0.45,
                0.045,
                0.45,
                true,
            ),
        };

        let (tint, energy) = {
            let e = &EMOTIONS[(emotion as usize).min(EMOTIONS.len() - 1)];
            let k = if emotion == 0 { 0.0 } else { 0.45 };
            ((e.1, k), e.2)
        };
        let swirl_v = swirl_v * energy;
        let plasma_v = plasma_v * energy;
        let emit_period_ms = (520.0 / energy) as i64;

        self.size = ease(self.size, t_size, dt, 140.0);
        self.glow = ease(self.glow, t_glow, dt, 200.0);
        self.orbit = ease(self.orbit, t_orbit, dt, 220.0);
        self.shimmer = ease(self.shimmer, t_shimmer, dt, 240.0);
        self.spin = ease(self.spin, t_spin, dt, 200.0);
        self.swirl = (self.swirl + swirl_v * dt / 1000.0) % 1.0;
        self.plasma = (self.plasma + plasma_v * dt / 1000.0) % 1.0;
        self.head = (self.head + self.spin * 0.22 * dt / 1000.0) % 1.0;

        // ---- expanding sound pulses ----
        if emit && t_ms - self.last_spawn_ms > emit_period_ms {
            self.last_spawn_ms = t_ms;
            self.pulses.push(Pulse {
                r: self.size + 4.0,
                alpha: 0.55,
            });
        }
        for p in &mut self.pulses {
            p.r += 55.0 * dt / 1000.0;
            p.alpha *= (-dt / 420.0).exp();
        }
        self.pulses.retain(|p| p.alpha > 0.015 && p.r < W as f32);

        self.render(buf, t, tint);
    }

    fn render(&mut self, buf: &mut [u32], t: f32, tint: ((u8, u8, u8), f32)) {
        buf.fill(BG);
        let pal = palette();
        let g = geo();
        let (cx, cy) = ((W as f32 - 1.0) / 2.0, (H as f32 - 1.0) / 2.0);
        let size = self.size;
        let glow_r = 42.0f32;
        let tinted = |c: u32| -> (f32, f32, f32) {
            let (tr, tg, tb) = tint.0;
            let k = tint.1;
            (
                channel(c, 16) + (tr as f32 - channel(c, 16)) * k,
                channel(c, 8) + (tg as f32 - channel(c, 8)) * k,
                channel(c, 0) + (tb as f32 - channel(c, 0)) * k,
            )
        };
        let reach = (size + glow_r)
            .max(self.pulses.last().map(|p| p.r + 3.0).unwrap_or(0.0))
            .min(W as f32);

        // highlight sits up-left of center, like a lit core
        let (hx, hy) = (cx - size * 0.22, cy - size * 0.28);
        let hl_r = size * 0.55;

        for i in 0..W * H {
            let d = g.d[i];
            if d > reach {
                continue;
            }
            let a = g.a[i];

            // ---- outer bloom ----
            let mut r;
            let mut gr;
            let mut b;
            let bloom = if d > size {
                (-(d - size) / glow_r * 2.2).exp() * self.glow
            } else {
                0.0
            };
            let ci = (((a - self.swirl) * 256.0).rem_euclid(256.0)) as usize % 256;
            let (base_r, base_g, base_b) = tinted(pal[ci]);
            if bloom > 0.0 {
                let k = bloom * 0.5;
                r = 8.0 + (base_r - 8.0) * k;
                gr = 8.0 + (base_g - 8.0) * k;
                b = 13.0 + (base_b - 13.0) * k;
            } else {
                r = 8.0;
                gr = 8.0;
                b = 13.0;
            }

            // ---- expanding pulse rings ----
            for p in &self.pulses {
                let band = (-(d - p.r) * (d - p.r) / 6.0).exp() * p.alpha;
                if band > 0.004 {
                    r += (base_r - r) * band * 0.6;
                    gr += (base_g - gr) * band * 0.6;
                    b += (base_b - b) * band * 0.6;
                }
            }

            // ---- solid orb body ----
            let wob = 1.0 + 0.018 * sin2pi(3.0 * a + t * 0.00045);
            let rw = size * wob;
            let body = ((rw - d) / 1.6).clamp(0.0, 1.0);
            if body > 0.0 {
                // angular gradient, radial shading: darker rim, lit interior
                let k = (d / rw).clamp(0.0, 1.0);
                let shade = 0.55 + 0.45 * (1.0 - k).powf(0.75);
                // ease the spokes out near the core with a fixed tint
                let core_mix = (1.0 - k).powi(2) * 0.65;
                let core = (138.0, 124.0, 255.0);
                // plasma shimmer bands (subtle, no hole)
                let shim = 1.0
                    - self.shimmer * 0.16
                        * (0.5 + 0.5 * sin2pi(3.0 * a + self.plasma + d / rw * 1.1));
                let (mut or_, mut og, mut ob) = {
                    let (cr0, cg0, cb0) = (base_r, base_g, base_b);
                    let cr = cr0 + (core.0 - cr0) * core_mix;
                    let cg = cg0 + (core.1 - cg0) * core_mix;
                    let cb = cb0 + (core.2 - cb0) * core_mix;
                    (cr * shade * shim, cg * shade * shim, cb * shade * shim)
                };
                // offset core highlight
                let hx = g.dx[i] - (hx - cx);
                let hy = g.dy[i] - (hy - cy);
                let hd2 = (hx * hx + hy * hy) / (hl_r * hl_r);
                let hl = (-hd2).exp() * 0.38;
                or_ += (255.0 - or_) * hl;
                og += (255.0 - og) * hl;
                ob += (255.0 - ob) * hl;
                // thin bright rim
                if d > rw - 1.8 {
                    or_ += (255.0 - or_) * 0.30;
                    og += (255.0 - og) * 0.30;
                    ob += (255.0 - ob) * 0.30;
                }
                r = r * (1.0 - body) + or_ * body;
                gr = gr * (1.0 - body) + og * body;
                b = b * (1.0 - body) + ob * body;
            }

            // ---- orbiting comet arcs (solid-space, around the orb) ----
            if self.orbit > 0.02 && d > size * 0.6 {
                for orbit_i in 0..2 {
                    let ra = size + 9.0 + orbit_i as f32 * 7.0;
                    let speed = if orbit_i == 0 { 1.0 } else { -0.65 };
                    let head = self.head * speed + orbit_i as f32 * 0.5;
                    let delta = (a - head).rem_euclid(1.0);
                    let span = 0.16 - orbit_i as f32 * 0.04;
                    if delta < span {
                        let fade = (1.0 - delta / span).powi(2);
                        let band = (-(d - ra) * (d - ra) / 4.0).exp();
                        let k = fade * band * self.orbit * (0.85 - orbit_i as f32 * 0.25);
                        if k > 0.004 {
                            let c2 = tinted(pal[((a + 0.25).rem_euclid(1.0) * 256.0) as usize % 256]);
                            r += (c2.0 - r) * k;
                            gr += (c2.1 - gr) * k;
                            b += (c2.2 - b) * k;
                        }
                    }
                }
            }

            buf[i] = pack(r, gr, b);
        }
    }
}

/// Write the buffer as a 24-bit BMP (for headless previews; rows are 4-byte
/// aligned at 240px wide so no padding is needed).
pub fn write_bmp(path: &std::path::Path, buf: &[u32]) -> std::io::Result<()> {
    use std::io::Write;
    assert_eq!(buf.len(), W * H);
    let (w, h) = (W as u32, H as u32);
    let data_size = w * h * 3;
    let mut f = std::fs::File::create(path)?;
    f.write_all(b"BM")?;
    f.write_all(&(54 + data_size).to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&54u32.to_le_bytes())?;
    f.write_all(&40u32.to_le_bytes())?;
    f.write_all(&w.to_le_bytes())?;
    f.write_all(&h.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&24u16.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&data_size.to_le_bytes())?;
    f.write_all(&2835u32.to_le_bytes())?;
    f.write_all(&2835u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    for y in (0..H).rev() {
        for x in 0..W {
            let px = buf[y * W + x];
            f.write_all(&[
                (px & 0xff) as u8,
                ((px >> 8) & 0xff) as u8,
                ((px >> 16) & 0xff) as u8,
            ])?;
        }
    }
    Ok(())
}
