// Release builds on Windows use the GUI subsystem so double-clicking the exe
// doesn't open a console window (debug builds keep one for development). A
// GUI-subsystem process has no console unless its stdio is redirected, so
// stdout/stderr writes become silent no-ops — logs live in the log file.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod actions;
mod audio;
mod config;
mod divination;
mod fenster;
mod hostinfo;
mod identity;
mod ota;
mod protocol;
mod state;
mod talk;
mod tray;
mod ui;

use config::Mode;
use state::{Phase, SharedState};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

const W: i32 = 240;
const H: i32 = 240;

/// Logs go to a file (config `log_file` > env RUOZHI_LOG_FILE > /tmp/ruozhi.log)
/// and are mirrored to stdout when one is attached, so `open`-launched GUI runs
/// stay visible on disk while terminal runs keep live output.
fn init_logger(config_log_file: Option<&str>) -> PathBuf {
    struct Tee {
        file: Option<std::fs::File>,
        mirror: bool,
    }
    impl Write for Tee {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(f) = self.file.as_mut() {
                let _ = f.write_all(buf);
            }
            if self.mirror {
                let mut out = std::io::stdout();
                let _ = out.write_all(buf);
                let _ = out.flush();
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if let Some(f) = self.file.as_mut() {
                f.flush()?;
            }
            if self.mirror {
                std::io::stdout().flush()?;
            }
            Ok(())
        }
    }

    // macOS keeps the firmware-ish /tmp path; elsewhere use the OS temp dir
    // (TMPDIR on Linux, %TEMP% on Windows).
    let path = std::env::var("RUOZHI_LOG_FILE")
        .map(PathBuf::from)
        .ok()
        .or_else(|| config_log_file.map(PathBuf::from))
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                PathBuf::from("/tmp/ruozhi.log")
            } else {
                std::env::temp_dir().join("ruozhi.log")
            }
        });

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| log::warn!("cannot open log file {}: {e}", path.display()))
        .ok();

    let mirror = std::io::IsTerminal::is_terminal(&std::io::stdout());
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(Tee { file, mirror })))
        .init();
    path
}

fn main() {
    let args = config::parse();
    let log_path = init_logger(args.config.log_file.as_deref());
    log::info!("=== ruozhi {} start, log: {} ===", env!("CARGO_PKG_VERSION"), log_path.display());
    if args.created_default_config {
        log::info!("first run: default config written to {}", config::Config::path().display());
    }
    let result = match args.mode {
        Mode::App => run_app(args.config.clone(), &args, audio::Source::Mic),
        Mode::Wav(ref path) => run_app(args.config.clone(), &args, audio::Source::Wav(path.clone())),
        Mode::Demo => run_demo(),
        Mode::Loopback => run_loopback(),
        Mode::RenderFrames(dir) => Ok(render_frames(&dir)),
    };
    if let Err(e) = result {
        log::error!("{e:#}");
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Relaunch this binary (via `open` when running inside a bundle so macOS
/// keeps the app identity/permissions).
fn restart_self() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(app) = exe.ancestors().find(|p| {
            p.extension().map(|e| e == "app").unwrap_or(false)
        }) {
            if std::process::Command::new("open").arg(app).spawn().is_ok() {
                std::process::exit(0);
            }
        }
        if std::process::Command::new(&exe)
            .args(std::env::args().skip(1))
            .spawn()
            .is_ok()
        {
            std::process::exit(0);
        }
    }
    std::process::exit(1);
}

/// The full hold-to-talk client.
fn run_app(cfg: config::Config, args: &config::Args, source: audio::Source) -> anyhow::Result<()> {
    let shared = Arc::new(SharedState::default());
    let volume = Arc::new(std::sync::atomic::AtomicU32::new(audio::DEFAULT_VOLUME));
    let conn = config::finalize(cfg, args, volume.clone())?;

    let handles = audio::spawn(shared.clone(), source, volume)?;
    let (ui_tx, ui_rx) = crossbeam_channel::unbounded();
    protocol::spawn(conn, shared.clone(), ui_rx, handles);

    let mut buf = vec![0u32; ui::W * ui::H];
    let mut f = fenster::Fenster::new("ruozhi", W, H, &mut buf);
    let mut orb = ui::Orb::new();
    f.open();
    // Menu-bar residency (macOS/Windows): closing the window only hides it,
    // the tray icon is the way back. Linux has no tray yet and keeps quitting.
    tray::init(&f);
    log::info!("ready — hold {} to talk, Esc to hide the window", fenster::talk_key_name());

    let mut prev_talk = false;
    let mut prev_esc = false;
    // MCP self.exit bookkeeping: when the request was seen, and when the
    // phase last settled into Idle
    let mut quit_seen: Option<std::time::Instant> = None;
    let mut idle_since: Option<std::time::Instant> = None;
    loop {
        let frame_start = fenster::time();

        // non-zero means the window is gone (WM_QUIT after the user closed it
        // on Windows — unreachable now that close hides instead of destroys,
        // kept as a safety net).
        if f.loop_once() != 0 {
            break;
        }

        // tray menu: "打开窗口" is applied inside poll(); Restart = "重启程序",
        // Quit = "关闭程序"
        match tray::poll() {
            tray::TrayCmd::Restart => {
                log::info!("restart requested from tray menu — restarting");
                tray::shutdown();
                f.close();
                restart_self();
            }
            tray::TrayCmd::Quit => {
                let _ = ui_tx.send(state::UiEvent::Quit);
                break;
            }
            _ => {}
        }

        // Esc hides the window into the tray (Linux: still quits).
        let esc = f.key(27);
        if esc && !prev_esc {
            if tray::available() {
                tray::hide();
                f.reset_input();
                // no forced CmdUp on hide: the talk key below is read from
                // the global keyboard state, so a held key simply keeps
                // talking while the window is hidden
            } else {
                let _ = ui_tx.send(state::UiEvent::Quit);
                break;
            }
        }
        prev_esc = esc;

        // talk key: OS-global keyboard state so holding works while the
        // window is hidden or another app is focused (fenster events only
        // arrive with focus; Linux without X11 falls back to window events)
        let talk = if talk::available() {
            talk::pressed()
        } else {
            f.talk_pressed()
        };
        if talk != prev_talk {
            let _ = ui_tx.send(if talk {
                state::UiEvent::CmdDown
            } else {
                state::UiEvent::CmdUp
            });
            prev_talk = talk;
        }

        // MCP self.reboot: relaunch this app once the reply has had time to flush
        if shared.action() == state::ACTION_RESTART {
            log::info!("reboot requested via MCP — restarting");
            std::thread::sleep(std::time::Duration::from_millis(300));
            tray::shutdown();
            f.close();
            restart_self();
        }

        // MCP self.exit: quit after the goodbye reply finishes. The turn ends
        // in Idle; require a short stable-idle window so a reply that starts
        // right after the tool call still plays out, capped at 30 s for turns
        // that never speak.
        if shared.action() == state::ACTION_QUIT {
            let t0 = *quit_seen.get_or_insert_with(std::time::Instant::now);
            if shared.phase() != Phase::Idle {
                idle_since = None;
            } else {
                let since = *idle_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > std::time::Duration::from_secs(2)
                    || t0.elapsed() > std::time::Duration::from_secs(30)
                {
                    log::info!("exit requested via MCP — quitting");
                    let _ = ui_tx.send(state::UiEvent::Quit);
                    break;
                }
            }
        }

        // nothing to see while the window is hidden — skip the pixel work
        if !tray::is_hidden() {
            orb.draw(
                &mut buf,
                shared.phase(),
                frame_start,
                shared.mic_level(),
                shared.level(),
                shared.emotion(),
            );
        }

        // Adaptive frame pace: 60 fps only pays for itself while a state
        // animation is running. Hidden, the loop just watches the tray icon
        // and the talk key (30 Hz keeps the press latency imperceptible);
        // a visible standby orb breathes fine at 20 fps. Cuts wakeups (and
        // the full-screen pixel work) sharply whenever nothing moves.
        let frame_ms = if tray::is_hidden() {
            33
        } else if shared.phase() == Phase::Idle {
            50
        } else {
            1000 / 60
        };
        let wait = frame_ms - (fenster::time() - frame_start);
        if wait > 0 {
            fenster::sleep(wait);
        }
    }
    tray::shutdown();
    f.close();
    talk::shutdown();
    Ok(())
}

/// Audio pipeline self-test: mic -> opus encode -> opus decode -> speakers.
/// The UI mirrors what the pipeline is doing (fire while mic is hot, smiley
/// while audio plays back). Esc quits.
fn run_loopback() -> anyhow::Result<()> {
    let shared = Arc::new(SharedState::default());
    let handles = audio::spawn(
        shared.clone(),
        audio::Source::Mic,
        Arc::new(std::sync::atomic::AtomicU32::new(audio::DEFAULT_VOLUME)),
    )?;
    // loopback is an explicit mic self-test: open capture right away (the OS
    // mic permission prompt appearing here is expected), no talk button.
    handles.ensure_capture();
    shared.set_mic_enabled(true);

    // route encoded packets straight into the playback queue
    let jitter = handles.jitter.clone();
    std::thread::spawn(move || {
        for pkt in handles.encoded_rx {
            jitter.push(pkt);
        }
    });

    let mut buf = vec![0u32; ui::W * ui::H];
    let mut f = fenster::Fenster::new("ruozhi loopback", W, H, &mut buf);
    let mut orb = ui::Orb::new();
    f.open();
    log::info!("loopback: speak into the mic, Esc to quit");

    loop {
        let frame_start = fenster::time();
        f.loop_once();
        if f.key(27) {
            break;
        }

        // derive the displayed phase from live pipeline levels
        let phase = if shared.level() > 0.004 {
            Phase::Speaking
        } else if shared.mic_level() > 0.015 {
            Phase::Listening
        } else {
            Phase::Idle
        };
        orb.draw(&mut buf, phase, frame_start, shared.mic_level(), shared.level(), shared.emotion());

        let wait = 1000 / 60 - (fenster::time() - frame_start);
        if wait > 0 {
            fenster::sleep(wait);
        }
    }
    f.close();
    Ok(())
}

/// Cycle through every phase in a live window, for eyeballing the UI.
fn run_demo() -> anyhow::Result<()> {
    let mut buf = vec![0u32; ui::W * ui::H];
    let mut f = fenster::Fenster::new("ruozhi demo", W, H, &mut buf);
    let mut orb = ui::Orb::new();

    // (phase, duration ms, fake level, emotion index)
    let script: Vec<(Phase, i64, f32, u8)> = vec![
        (Phase::Idle, 2500, 0.0, 0),
        (Phase::Connecting, 1500, 0.0, 0),
        (Phase::Listening, 5000, 0.0, 0),
        (Phase::Waiting, 4000, 0.0, 0),
        (Phase::Speaking, 5000, 0.05, 9),  // thinking violet
    ];

    f.open();
    let mut t0 = fenster::time();
    loop {
        let frame_start = fenster::time();
        // pump events (and repaint); a closed window (WM_QUIT) ends the demo
        if f.loop_once() != 0 {
            break;
        }
        if f.key(27) {
            break;
        }
        let mut elapsed = frame_start - t0;
        for &(phase, dur, level, emo) in &script {
            if elapsed < dur {
                orb.draw(&mut buf, phase, frame_start, 0.02, level, emo);
                break;
            }
            elapsed -= dur;
            t0 += dur;
        }
        if frame_start - t0 >= script.iter().map(|&(_, d, _, _)| d).sum::<i64>() {
            t0 = frame_start;
        }

        let wait = 1000 / 60 - (fenster::time() - frame_start);
        if wait > 0 {
            fenster::sleep(wait);
        }
    }
    f.close();
    Ok(())
}

/// Headless preview: settle each phase's animation, then dump a BMP.
fn render_frames(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    let mut buf = vec![0u32; ui::W * ui::H];
    let mut orb = ui::Orb::new();
    let mut t = 1000;

    let mut snap = |orb: &mut ui::Orb, phase: Phase, mic: f32, play: f32, emo: u8, name: &str| {
        // run the animation forward so eased parameters settle
        for _ in 0..50 {
            t += 22;
            orb.draw(&mut buf, phase, t, mic, play, emo);
        }
        ui::write_bmp(&dir.join(format!("{name}.bmp")), &buf).unwrap();
    };

    snap(&mut orb, Phase::Idle, 0.0, 0.0, 0, "idle");
    snap(&mut orb, Phase::Connecting, 0.0, 0.0, 0, "connecting");
    snap(&mut orb, Phase::Listening, 0.05, 0.0, 0, "listening");
    snap(&mut orb, Phase::Waiting, 0.0, 0.0, 0, "waiting");
    snap(&mut orb, Phase::Speaking, 0.0, 0.05, 0, "speaking");
    snap(&mut orb, Phase::Speaking, 0.0, 0.05, 1, "speaking-happy");
    snap(&mut orb, Phase::Speaking, 0.0, 0.05, 3, "speaking-sad");

    println!("frames written to {}", dir.display());
}
