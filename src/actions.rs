//! Local system actions behind the MCP tools: open an app/URL/file, lock the
//! screen. Deliberately small: one well-scoped call per platform, no shell
//! strings (injection-safe by construction), and the protocol thread logs
//! every invocation.

/// Open a URL (any `scheme://...`), an app by name, or a file/folder path.
/// Returns success.
pub fn open(target: &str) -> bool {
    let target = target.trim();
    if target.is_empty() {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        // `open` handles URLs, apps (-a) and files/folders; no shell involved.
        let ok = |args: &[&str]| {
            std::process::Command::new("open")
                .args(args)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if target.contains("://") {
            ok(&[target])
        } else {
            ok(&["-a", target]) || ok(&[target])
        }
    }
    #[cfg(windows)]
    {
        shell_open(target)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // URLs and files; desktop-app names are DE-specific and not handled
        std::process::Command::new("xdg-open")
            .arg(target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Lock the screen. macOS: display sleep (locks under the default
/// password-after-sleep setting); Windows: LockWorkStation; Linux: loginctl.
pub fn lock_screen() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("pmset")
            .arg("displaysleepnow")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        unsafe { LockWorkStation() != 0 }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("loginctl")
            .args(["lock-session"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    /// ShellExecuteW = the exact "double-click" semantics for URLs, app names
    /// and documents, without going through a shell string.
    #[link(name = "shell32")]
    unsafe extern "system" {
        pub fn ShellExecuteW(
            hwnd: *mut c_void,
            op: *const u16,
            file: *const u16,
            params: *const u16,
            dir: *const u16,
            show: i32,
        ) -> usize;
    }

    /// LockWorkStation (user32, already linked by build.rs).
    #[link(name = "user32")]
    unsafe extern "system" {
        pub fn LockWorkStation() -> i32;
    }
}

#[cfg(windows)]
use win::{LockWorkStation, ShellExecuteW};

#[cfg(windows)]
fn shell_open(target: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let wide = |s: &str| {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let op = wide("open");
    let file = wide(target);
    // returns an HINSTANCE-like value; >32 means success
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    rc > 32
}
