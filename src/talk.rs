//! Global hold-to-talk key state.
//!
//! Window key events (fenster) only arrive while the window has focus, but
//! hold-to-talk must keep working while the window is hidden in the tray —
//! and while other apps are focused. So the talk key is read from the
//! OS-wide keyboard state every frame instead:
//!
//! - macOS: `CGEventSourceFlagsState` — the physical Command state, a
//!   passive query that needs no Accessibility/Input-Monitoring permission
//!   (unlike event taps or NSEvent global monitors)
//! - Windows: `GetAsyncKeyState(VK_CONTROL)` — global for desktop apps
//! - Linux/X11: `XQueryKeymap` — the server-wide key bitmap over a private
//!   X connection; unavailable without X11 (pure Wayland), where
//!   [`available`] reports false and the caller falls back to window events

#[cfg(target_os = "macos")]
mod imp {
    /// kCGEventFlagMaskCommand.
    const FLAG_COMMAND: u64 = 1 << 20;
    /// kCGEventSourceStateHIDSystemState: the physical keyboard, regardless
    /// of which app has focus.
    const HID_SYSTEM: u32 = 1;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceFlagsState(state_id: u32) -> u64;
    }

    pub fn available() -> bool {
        true
    }

    pub fn pressed() -> bool {
        unsafe { CGEventSourceFlagsState(HID_SYSTEM) & FLAG_COMMAND != 0 }
    }

    pub fn shutdown() {}
}

#[cfg(windows)]
mod imp {
    const VK_CONTROL: i32 = 0x11;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetAsyncKeyState(v_key: i32) -> u16;
    }

    pub fn available() -> bool {
        true
    }

    pub fn pressed() -> bool {
        // high bit = currently held
        unsafe { GetAsyncKeyState(VK_CONTROL) & 0x8000 != 0 }
    }

    pub fn shutdown() {}
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::ffi::c_char;
    use std::sync::OnceLock;

    // XK_Control_L / XK_Control_R
    const XK_CONTROL_L: usize = 0xffe5;
    const XK_CONTROL_R: usize = 0xffe4;

    unsafe extern "C" {
        fn XOpenDisplay(name: *const c_char) -> usize;
        fn XCloseDisplay(dpy: usize) -> i32;
        fn XQueryKeymap(dpy: usize, keys: *mut u8) -> i32;
        fn XKeysymToKeycode(dpy: usize, keysym: usize) -> u8;
    }

    /// Our own X connection. Xlib types are not Send, so the raw pointer is
    /// parked as usize; only the main thread ever touches it.
    static DISPLAY: OnceLock<usize> = OnceLock::new();

    fn display() -> Option<usize> {
        match *DISPLAY.get_or_init(|| {
            let dpy = unsafe { XOpenDisplay(std::ptr::null()) };
            if dpy == 0 {
                log::info!(
                    "no global key state (X11 unavailable — Wayland?); \
                     the talk key only works while the window is focused"
                );
            }
            dpy
        }) {
            0 => None,
            dpy => Some(dpy),
        }
    }

    /// The two Control keycodes, resolved once against the server keymap.
    fn control_keycodes(dpy: usize) -> (u8, u8) {
        static CODES: OnceLock<(u8, u8)> = OnceLock::new();
        *CODES.get_or_init(|| unsafe {
            (XKeysymToKeycode(dpy, XK_CONTROL_L), XKeysymToKeycode(dpy, XK_CONTROL_R))
        })
    }

    pub fn available() -> bool {
        display().is_some()
    }

    pub fn pressed() -> bool {
        let Some(dpy) = display() else {
            return false;
        };
        let mut keys = [0u8; 32];
        if unsafe { XQueryKeymap(dpy, keys.as_mut_ptr()) } == 0 {
            return false;
        }
        // key K lives at bit (K % 8) of byte (K / 8), LSB first
        let down = |kc: u8| kc >= 8 && (keys[(kc >> 3) as usize] >> (kc & 7)) & 1 != 0;
        let (l, r) = control_keycodes(dpy);
        down(l) || down(r)
    }

    pub fn shutdown() {
        if let Some(&dpy) = DISPLAY.get() {
            if dpy != 0 {
                unsafe { XCloseDisplay(dpy) };
            }
        }
    }
}

/// Whether a global key-state source exists on this platform/session.
pub fn available() -> bool {
    imp::available()
}

/// Whether the hold-to-talk key is currently held, globally.
pub fn pressed() -> bool {
    imp::pressed()
}

/// Release the global key-state source (the X11 connection on Linux).
pub fn shutdown() {
    imp::shutdown()
}
