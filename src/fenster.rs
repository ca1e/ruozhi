//! Hand-written FFI bindings for the fenster window library
//! (vendored at c/fenster.h, compiled by build.rs).
//!
//! The raw struct layout must match `struct fenster` in fenster.h. On macOS
//! the trailing platform field is `id wnd`, i.e. a plain pointer.

use std::ffi::{c_char, c_int, c_void, CString};

/// Modifier bit mask stored in `mod`: ctrl=1, shift=2, alt=4, meta=8.
/// macOS Command key is meta (Windows: Win, Linux: Super); only macOS's
/// TALK_MOD aliases it, so the constant itself is mac-only.
#[cfg(target_os = "macos")]
pub const MOD_META: c_int = 8;
#[allow(dead_code)] // used as TALK_MOD on non-mac platforms
pub const MOD_CTRL: c_int = 1;

/// The hold-to-talk modifier: Command on macOS; Ctrl elsewhere. The meta key
/// on Windows/Linux is the OS key (Win/Super) — releasing it pops the start
/// menu / activities overview, so it can't be held to talk.
#[cfg(target_os = "macos")]
pub const TALK_MOD: c_int = MOD_META;
#[cfg(not(target_os = "macos"))]
pub const TALK_MOD: c_int = MOD_CTRL;

/// Display name of the hold-to-talk key, for logs and hints.
pub fn talk_key_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Command"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "Ctrl"
    }
}

#[repr(C)]
struct FenRaw {
    title: *const c_char,
    width: c_int,
    height: c_int,
    buf: *mut u32,
    keys: [c_int; 256],
    r#mod: c_int,
    x: c_int,
    y: c_int,
    mouse: c_int,
    // Must mirror `struct fenster` in c/fenster.h exactly: Linux carries four
    // pointer-sized fields there (Display*, Window, GC, XImage*); the others
    // carry one.
    #[cfg(target_os = "linux")]
    x11: [usize; 4],
    #[cfg(not(target_os = "linux"))]
    wnd: *mut c_void,
}

/// Owned handle; `_title` keeps the C string alive as long as the window does.
pub struct Fenster {
    raw: FenRaw,
    _title: CString,
}

impl Fenster {
    /// `buf.len()` must equal `width * height`.
    pub fn new(title: &str, width: i32, height: i32, buf: &mut [u32]) -> Self {
        debug_assert_eq!(buf.len(), (width * height) as usize);
        let c_title = CString::new(title).expect("window title contains NUL");
        let raw = FenRaw {
            title: c_title.as_ptr(),
            width,
            height,
            buf: buf.as_mut_ptr(),
            keys: [0; 256],
            r#mod: 0,
            x: 0,
            y: 0,
            mouse: 0,
            #[cfg(target_os = "linux")]
            x11: [0; 4],
            #[cfg(not(target_os = "linux"))]
            wnd: std::ptr::null_mut(),
        };
        Self {
            raw,
            _title: c_title,
        }
    }

    pub fn key(&self, code: usize) -> bool {
        // Key state is polled per frame: 1 = currently held. Indexes are mostly
        // uppercase ASCII (Esc = 27).
        self.raw.keys[code.min(255)] != 0
    }

    /// True while the hold-to-talk key is held ([`TALK_MOD`]: Command on
    /// macOS — needs the FlagsChanged patch vendored into fenster.h — and
    /// Ctrl on Windows/Linux, which report plain key events).
    ///
    /// Fallback only: the main loop reads the OS-global key state
    /// (`talk::pressed`) so the key also works while the window is hidden
    /// or unfocused; this window-event path is used when no global source
    /// exists (Linux without X11).
    pub fn talk_pressed(&self) -> bool {
        self.raw.r#mod & TALK_MOD != 0
    }

    pub fn open(&mut self) -> i32 {
        unsafe { fenster_open(&mut self.raw) }
    }

    /// Platform window handle (macOS: NSWindow `id`; Windows: HWND; null on
    /// Linux). Valid after [`Fenster::open`].
    pub fn window_handle(&self) -> *mut c_void {
        #[cfg(target_os = "linux")]
        {
            std::ptr::null_mut()
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.raw.wnd
        }
    }

    /// Drops polled input state whose release a hidden window will never see:
    /// once hidden, no more key events arrive, so a held Esc / modifier would
    /// otherwise stay latched forever.
    pub fn reset_input(&mut self) {
        self.raw.keys[27] = 0; // Esc
        self.raw.r#mod = 0;
    }

    /// Pumps one frame: draws the buffer and processes at most one event.
    pub fn loop_once(&mut self) -> i32 {
        unsafe { fenster_loop(&mut self.raw) }
    }

    pub fn close(&mut self) {
        unsafe { fenster_close(&mut self.raw) }
    }
}

pub fn sleep(ms: i64) {
    unsafe { fenster_sleep(ms) }
}

pub fn time() -> i64 {
    unsafe { fenster_time() }
}

unsafe extern "C" {
    fn fenster_open(f: *mut FenRaw) -> c_int;
    fn fenster_loop(f: *mut FenRaw) -> c_int;
    fn fenster_close(f: *mut FenRaw);
    fn fenster_sleep(ms: i64);
    fn fenster_time() -> i64;
}
