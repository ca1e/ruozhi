//! Host machine information for the MCP `self.get_device_info` tool:
//! OS version, CPU, memory and GPU. Hardware facts don't change while the
//! app runs, so the JSON is collected once and cached.

use std::sync::OnceLock;

/// JSON string with the host summary (os / kernel / cpu / memory / gpu).
pub fn json() -> &'static str {
    static INFO: OnceLock<String> = OnceLock::new();
    INFO.get_or_init(collect)
}

fn collect() -> String {
    let sys = sysinfo::System::new_all();

    // marketing OS name/version (sw_vers on macOS), falling back to sysinfo's
    // long name ("Windows 11 Pro") and then its kernel-ish naming
    let os = macos_product_version().unwrap_or_else(|| {
        sysinfo::System::long_os_version().unwrap_or_else(|| {
            format!(
                "{} {}",
                sysinfo::System::name().unwrap_or_else(|| "unknown".into()),
                sysinfo::System::os_version().unwrap_or_default()
            )
        })
    });
    let kernel = sysinfo::System::kernel_version().unwrap_or_default();
    let cpus = sys.cpus();
    let cpu_brand = cpus
        .first()
        .map(|c| c.brand().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let cpu = format!(
        "{} ({} cores / {} threads)",
        cpu_brand,
        sysinfo::System::physical_core_count().unwrap_or(cpus.len()),
        cpus.len()
    );

    let gib = |b: u64| (b as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0;
    let mem_total = gib(sys.total_memory());
    let mem_used = gib(sys.used_memory());

    let gpus = gpus().join(", ");
    serde_json::json!({
        "os": os,
        "kernel": kernel,
        "cpu": cpu,
        "memory_total_gb": mem_total,
        "memory_used_gb": mem_used,
        "gpu": gpus,
    })
    .to_string()
}

/// "macOS 26.1" via sw_vers (None off-macOS or if the tool is missing).
#[cfg(target_os = "macos")]
fn macos_product_version() -> Option<String> {
    fn sw(flag: &str) -> Option<String> {
        let out = std::process::Command::new("sw_vers").arg(flag).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then_some(s)
    }
    let name = sw("-productName")?;
    let version = sw("-productVersion")?;
    Some(format!("{name} {version}"))
}

#[cfg(not(target_os = "macos"))]
fn macos_product_version() -> Option<String> {
    None
}

/// GPU summary. macOS asks `system_profiler` (sysinfo has no GPU info);
/// Windows asks CIM via PowerShell (wmic is removed on recent Win11 builds);
/// other platforms fall back to a cheap best-effort or "unknown".
#[cfg(target_os = "macos")]
fn gpus() -> Vec<String> {
    let out = std::process::Command::new("system_profiler")
        .args(["SPDisplaysDataType", "-json"])
        .output();
    let Ok(out) = out else {
        return vec!["unknown".into()];
    };
    let Ok(text) = String::from_utf8(out.stdout) else {
        return vec!["unknown".into()];
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return vec!["unknown".into()];
    };

    let mut gpus = Vec::new();
    if let Some(items) = v
        .pointer("/SPDisplaysDataType")
        .and_then(|x| x.as_array())
    {
        for item in items {
            let model = item
                .get("sppci_model")
                .or_else(|| item.get("_name"))
                .and_then(|m| m.as_str())
                .unwrap_or("GPU");
            let vram = item
                .get("spdisplays_vram_shared")
                .or_else(|| item.get("spdisplays_vram"))
                .and_then(|m| m.as_str())
                .map(|v| format!(" ({v})"))
                .unwrap_or_default();
            gpus.push(format!("{model}{vram}"));
        }
    }
    if gpus.is_empty() {
        gpus.push("unknown".into());
    }
    gpus
}

#[cfg(windows)]
fn gpus() -> Vec<String> {
    // Win32_VideoController.Name via PowerShell; the `| join` keeps the
    // output to one line so multi-GPU machines parse trivially.
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name }) -join ';'",
        ])
        .output();
    let Ok(out) = out else {
        return vec!["unknown".into()];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let list: Vec<String> = text
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if list.is_empty() {
        vec!["unknown".into()]
    } else {
        list
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn gpus() -> Vec<String> {
    // best effort via lspci where available
    if let Ok(out) = std::process::Command::new("sh")
        .args(["-c", "lspci 2>/dev/null | grep -iE 'vga|3d|display' | cut -d: -f3"])
        .output()
    {
        let list: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    vec!["unknown".into()]
}

// ---------------------------------------------------------------------------
// Battery (laptop)
// ---------------------------------------------------------------------------

/// (percent 0..100, charging) of the first battery; None on desktops.
pub fn battery() -> Option<(u32, bool)> {
    use starship_battery::Manager;
    let manager = Manager::new().ok()?;
    let battery = manager.batteries().ok()?.next()?.ok()?;
    let percent = (battery.state_of_charge().get::<starship_battery::units::ratio::percent>()).clamp(0.0, 100.0);
    let charging = battery.state() == starship_battery::State::Charging;
    Some((percent as u32, charging))
}

// ---------------------------------------------------------------------------
// Local network address
// ---------------------------------------------------------------------------

/// Primary local IPv4 address.
pub fn local_ip() -> Option<String> {
    local_ip_address::local_ip().ok().map(|ip| ip.to_string())
}

// ---------------------------------------------------------------------------
// Screen brightness
//
// Every platform gets real get/set, hiding behind get_brightness() (0..=1,
// None when unavailable) and set_brightness(0..=1) -> bool.
//
// macOS: on Apple-Silicon the internal panel is driven by DCP; the classic
//   IOKit IODisplayConnect path enumerates nothing there. The working
//   interface is CoreDisplay's user-brightness pair (same approach as Lunar
//   and friends), resolved with dlopen so a future macOS removal degrades
//   gracefully.
// Windows: two complementary paths, tried in turn — DDC/CI (dxva2.dll) drives
//   external desktop monitors over the video cable, WMI
//   (WmiMonitorBrightness, via PowerShell) drives laptop panels that don't
//   speak DDC/CI.
// Linux/BSD: /sys/class/backlight — reads are world-readable, writes need
//   video-group/root permission, so setting falls back to a gdbus call on
//   GNOME's power daemon (KDE/Unity expose the same interface).
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(
        max: u32,
        displays: *mut u32,
        count: *mut u32,
    ) -> i32;
}

#[cfg(target_os = "macos")]
type GetUserBrightness = unsafe extern "C" fn(u32) -> f64;
#[cfg(target_os = "macos")]
type SetUserBrightness = unsafe extern "C" fn(u32, f64);

#[cfg(target_os = "macos")]
fn core_display_symbols() -> Option<(GetUserBrightness, SetUserBrightness)> {
    use std::ffi::CString;
    unsafe extern "C" {
        fn dlopen(path: *const std::ffi::c_char, mode: i32) -> *mut std::ffi::c_void;
        fn dlsym(handle: *mut std::ffi::c_void, name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    }
    let path = CString::new("/System/Library/Frameworks/CoreDisplay.framework/CoreDisplay").ok()?;
    let lib = unsafe { dlopen(path.as_ptr(), 1) }; // RTLD_LAZY
    if lib.is_null() {
        return None;
    }
    let sym = |name: &str| -> Option<*mut std::ffi::c_void> {
        let c = CString::new(name).ok()?;
        let p = unsafe { dlsym(lib, c.as_ptr()) };
        (!p.is_null()).then_some(p)
    };
    let get = unsafe { std::mem::transmute::<*mut std::ffi::c_void, GetUserBrightness>(sym("CoreDisplay_Display_GetUserBrightness")?) };
    let set = unsafe { std::mem::transmute::<*mut std::ffi::c_void, SetUserBrightness>(sym("CoreDisplay_Display_SetUserBrightness")?) };
    Some((get, set))
}

#[cfg(target_os = "macos")]
fn primary_display() -> Option<u32> {
    let mut id = 0u32;
    let mut count = 0u32;
    let rc = unsafe { CGGetActiveDisplayList(1, &mut id, &mut count) };
    (rc == 0 && count > 0).then_some(id)
}

/// Current user brightness of the main display, 0..=1.
#[cfg(target_os = "macos")]
pub fn get_brightness() -> Option<f32> {
    let Some((get, _)) = core_display_symbols() else {
        return None;
    };
    let Some(id) = primary_display() else {
        return None;
    };
    let v = unsafe { get(id) };
    if (0.0..=1.5).contains(&v) {
        Some(v.clamp(0.0, 1.0) as f32)
    } else {
        None
    }
}

/// Set user brightness of the main display, 0..=1. Returns success.
#[cfg(target_os = "macos")]
pub fn set_brightness(v: f32) -> bool {
    let Some((_, set)) = core_display_symbols() else {
        return false;
    };
    let Some(id) = primary_display() else {
        return false;
    };
    unsafe { set(id, v.clamp(0.05, 1.0) as f64) };
    true
}

// ---- Windows: DDC/CI (external monitors) + WMI (laptop panels) ----

#[cfg(windows)]
use std::ffi::c_void;

#[cfg(windows)]
const MONITOR_DEFAULTTOPRIMARY: u32 = 1;

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct WinPoint {
    x: i32,
    y: i32,
}

/// PHYSICAL_MONITOR from dxva2.h: handle + 128-WCHAR description.
#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct WinPhysicalMonitor {
    handle: *mut c_void,
    description: [u16; 128],
}

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn MonitorFromPoint(pt: WinPoint, flags: u32) -> *mut c_void;
}

// raw-dylib binds straight to dxva2.dll at link time — no import library
// needed, so the MSVC toolchain alone is enough.
#[cfg(windows)]
#[link(name = "dxva2", kind = "raw-dylib")]
unsafe extern "system" {
    fn GetNumberOfPhysicalMonitorsFromHMONITOR(
        hmonitor: *mut c_void,
        count: *mut u32,
    ) -> i32;
    fn GetPhysicalMonitorsFromHMONITOR(
        hmonitor: *mut c_void,
        size: u32,
        monitors: *mut WinPhysicalMonitor,
    ) -> i32;
    fn GetMonitorBrightness(
        handle: *mut c_void,
        min: *mut u32,
        cur: *mut u32,
        max: *mut u32,
    ) -> i32;
    fn SetMonitorBrightness(handle: *mut c_void, value: u32) -> i32;
    fn DestroyPhysicalMonitors(count: u32, monitors: *mut WinPhysicalMonitor) -> i32;
}

/// Run `f` on the first physical monitor behind the primary display, with the
/// enumerate/destroy bookkeeping handled. False when there is nothing to
/// drive (virtual display, RDP session) or the API reports failure.
#[cfg(windows)]
fn win_with_physical_monitor(f: impl FnOnce(*mut c_void) -> bool) -> bool {
    unsafe {
        let hmon = MonitorFromPoint(WinPoint { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if hmon.is_null() {
            return false;
        }
        let mut count = 0u32;
        if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut count) == 0 || count == 0 {
            return false;
        }
        let mut monitors = vec![
            WinPhysicalMonitor {
                handle: std::ptr::null_mut(),
                description: [0; 128],
            };
            count as usize
        ];
        let mut ok = false;
        if GetPhysicalMonitorsFromHMONITOR(hmon, count, monitors.as_mut_ptr()) != 0 {
            ok = f(monitors[0].handle);
            DestroyPhysicalMonitors(count, monitors.as_mut_ptr());
        }
        ok
    }
}

/// DDC/CI brightness of the primary monitor, 0..=1.
#[cfg(windows)]
fn win_ddc_get() -> Option<f32> {
    let mut out = None;
    win_with_physical_monitor(|h| unsafe {
        let (mut min, mut cur, mut max) = (0u32, 0u32, 0u32);
        if GetMonitorBrightness(h, &mut min, &mut cur, &mut max) == 0 || max <= min {
            return false;
        }
        out = Some((cur - min) as f32 / (max - min) as f32);
        true
    });
    out
}

#[cfg(windows)]
fn win_ddc_set(v: f32) -> bool {
    win_with_physical_monitor(|h| unsafe {
        let (mut min, mut cur, mut max) = (0u32, 0u32, 0u32);
        if GetMonitorBrightness(h, &mut min, &mut cur, &mut max) == 0 || max <= min {
            return false;
        }
        let value = min + (v.clamp(0.0, 1.0) * (max - min) as f32).round() as u32;
        SetMonitorBrightness(h, value.min(max)) != 0
    })
}

/// One-shot PowerShell query, trimmed stdout (None on failure or no output).
#[cfg(windows)]
fn powershell(script: &str) -> Option<String> {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// WMI path: laptop internal panels usually don't speak DDC/CI but do expose
/// WmiMonitorBrightness (root/wmi), reported as 0..=100.
#[cfg(windows)]
fn win_wmi_get() -> Option<f32> {
    let s = powershell(
        "(Get-CimInstance -Namespace root/wmi -ClassName WmiMonitorBrightness \
         | Select-Object -First 1).CurrentBrightness",
    )?;
    s.parse::<f32>()
        .ok()
        .map(|v| (v / 100.0).clamp(0.0, 1.0))
}

#[cfg(windows)]
fn win_wmi_set(v: f32) -> bool {
    let pct = (v * 100.0).round().clamp(0.0, 100.0);
    powershell(&format!(
        "(Get-CimInstance -Namespace root/wmi -ClassName WmiMonitorBrightnessMethods \
         | Select-Object -First 1).WmiSetBrightness(0, {pct})"
    ))
    .is_some()
}

/// DDC/CI first (external monitors), WMI second (laptop panels).
#[cfg(windows)]
pub fn get_brightness() -> Option<f32> {
    win_ddc_get()
        .or_else(win_wmi_get)
        .filter(|v| (0.0..=1.0).contains(v))
}

#[cfg(windows)]
pub fn set_brightness(v: f32) -> bool {
    win_ddc_set(v) || win_wmi_set(v)
}

// ---- Linux/BSD: kernel backlight + GNOME power-daemon fallback ----

/// First usable backlight device, e.g. /sys/class/backlight/intel_backlight.
#[cfg(all(unix, not(target_os = "macos")))]
fn backlight_device() -> Option<std::path::PathBuf> {
    std::fs::read_dir("/sys/class/backlight")
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.join("max_brightness").exists() && p.join("brightness").exists())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn backlight_read_u32(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// Kernel backlight level, 0..=1. Reads are world-readable, so this always
/// answers when a backlight device exists.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn get_brightness() -> Option<f32> {
    let dev = backlight_device()?;
    let max = backlight_read_u32(&dev.join("max_brightness"))?.max(1);
    let cur = backlight_read_u32(&dev.join("brightness"))?;
    Some((cur as f32 / max as f32).clamp(0.0, 1.0))
}

/// Writing sysfs needs video-group/root permission; desktop sessions usually
/// lack it, so fall back to GNOME's power daemon over gdbus.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn set_brightness(v: f32) -> bool {
    let v = v.clamp(0.0, 1.0);
    if let Some(dev) = backlight_device() {
        let max = backlight_read_u32(&dev.join("max_brightness"))
            .unwrap_or(255)
            .max(1);
        // never write 0 — many panels treat it as "backlight off"
        let value = (v * max as f32).round().clamp(1.0, max as f32) as u32;
        if std::fs::write(dev.join("brightness"), value.to_string()).is_ok() {
            return true;
        }
    }
    let pct = (v * 100.0).round().clamp(0.0, 100.0);
    std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.gnome.SettingsDaemon.Power",
            "--object-path",
            "/org/gnome/SettingsDaemon/Power",
            "--method",
            "org.gnome.SettingsDaemon.Power.Screen.SetPercentage",
            &pct.to_string(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn brightness_roundtrip() {
        let Some(orig) = super::get_brightness() else {
            eprintln!("brightness unsupported here; skipping");
            return;
        };
        assert!(super::set_brightness(orig), "set to current value should succeed");
        let again = super::get_brightness().unwrap();
        assert!((again - orig).abs() < 0.05, "brightness drifted: {orig} -> {again}");
    }

    // Read-only on purpose: cargo test must never touch the user's monitor.
    #[cfg(windows)]
    #[test]
    fn brightness_get_is_sane_when_supported() {
        match super::get_brightness() {
            Some(v) => assert!((0.0..=1.0).contains(&v), "brightness out of range: {v}"),
            None => eprintln!("brightness unsupported here (virtual display?); skipping"),
        }
    }

    // Same roundtrip as the macOS test above, but opt-in (`cargo test
    // brightness -- --ignored`) since it performs a real DDC/CI write.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn brightness_set_roundtrip_manual() {
        let Some(orig) = super::get_brightness() else {
            eprintln!("brightness unsupported here; skipping");
            return;
        };
        assert!(super::set_brightness(orig), "set to current value should succeed");
    }

    #[test]
    fn emotion_index_lookup() {
        assert_eq!(super::super::ui::emotion_index("happy"), 1);
        assert_eq!(super::super::ui::emotion_index("unknown-thing"), 0);
    }

    #[test]
    fn device_info_is_valid_json_with_expected_keys() {
        let v: serde_json::Value = serde_json::from_str(super::json()).expect("valid json");
        for key in ["os", "kernel", "cpu", "memory_total_gb", "gpu"] {
            assert!(v.get(key).is_some(), "missing key {key}");
        }
        assert!(v["memory_total_gb"].as_f64().unwrap() > 0.0);
    }
}
