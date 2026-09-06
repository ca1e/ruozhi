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
        .unwrap_or("unknown".into());
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
// System load / storage / network for the MCP query tools
// ---------------------------------------------------------------------------

fn gib(b: u64) -> f64 {
    (b as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0
}

/// Overall CPU/memory usage plus the top CPU-consuming processes, as JSON.
/// CPU numbers need two refreshes a beat apart, so a call costs ~250 ms.
pub fn top_processes() -> serde_json::Value {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys.refresh_cpu_usage();
    sys.refresh_memory();

    let mut procs: Vec<_> = sys
        .processes()
        .values()
        .map(|p| (p.name().to_string_lossy().into_owned(), p.cpu_usage(), p.memory()))
        .collect();
    procs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let top = procs
        .into_iter()
        .filter(|(_, cpu, _)| *cpu > 0.05)
        .take(5)
        .map(|(name, cpu, mem)| {
            serde_json::json!({
                "name": name,
                "cpu": (cpu * 10.0).round() / 10.0,
                "mem_mb": mem / (1024 * 1024),
            })
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "cpu_percent": (sys.global_cpu_usage() * 10.0).round() / 10.0,
        "memory_used_gb": gib(sys.used_memory()),
        "memory_total_gb": gib(sys.total_memory()),
        "top_by_cpu": top,
    })
}

/// Mounted disks with usage, biggest first (sub-GB volumes trimmed as noise).
pub fn storage() -> serde_json::Value {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut list: Vec<serde_json::Value> = disks
        .list()
        .iter()
        .map(|d| {
            serde_json::json!({
                "mount": d.mount_point().to_string_lossy(),
                "total_gb": gib(d.total_space()),
                "free_gb": gib(d.available_space()),
            })
        })
        .filter(|v| v["total_gb"].as_f64().unwrap_or(0.0) >= 1.0)
        .collect();
    list.sort_by(|a, b| {
        b["total_gb"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["total_gb"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    list.truncate(5);
    serde_json::json!({ "disks": list })
}

/// Network status: online (the tool call itself arrives over the wire), local
/// IP, and Wi-Fi SSID/signal where the OS discloses them.
pub fn network_status() -> serde_json::Value {
    let mut v = serde_json::json!({ "online": true });
    if let Some(ip) = local_ip() {
        v["ip"] = serde_json::json!(ip);
    }
    if let Some((ssid, signal)) = wifi() {
        let mut w = serde_json::json!({ "connected": true });
        if let Some(ssid) = ssid {
            w["ssid"] = serde_json::json!(ssid);
        }
        if let Some(signal) = signal {
            w["signal"] = serde_json::json!(signal);
        }
        v["wifi"] = w;
    }
    v
}

/// (ssid, signal %) of the associated Wi-Fi network. `ssid` is None when the
/// OS withholds the name — macOS redacts it as `<redacted>` without Location
/// permission, which we report as connected-without-name. None overall when
/// there is no Wi-Fi association (ethernet / no adapter).
#[cfg(target_os = "macos")]
fn wifi() -> Option<(Option<String>, Option<u8>)> {
    for iface in ["en0", "en1", "en2"] {
        let out = std::process::Command::new("ipconfig")
            .args(["getsummary", iface])
            .output();
        let Ok(out) = out else { continue };
        if !out.status.success() {
            continue;
        }
        if let Some(ssid) = parse_ssid_field(&String::from_utf8_lossy(&out.stdout)) {
            return Some(((ssid != "<redacted>").then_some(ssid), None));
        }
    }
    None
}

#[cfg(windows)]
fn wifi() -> Option<(Option<String>, Option<u8>)> {
    let out = std::process::Command::new("netsh")
        .args(["wlan", "show", "interfaces"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let ssid = parse_ssid_field(&text);
    let signal = parse_signal_field(&text);
    (ssid.is_some() || signal.is_some()).then_some((ssid, signal))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn wifi() -> Option<(Option<String>, Option<u8>)> {
    let out = std::process::Command::new("nmcli")
        .args(["-t", "-f", "ACTIVE,SSID,SIGNAL", "dev", "wifi"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split(':');
        if parts.next() != Some("yes") {
            continue;
        }
        let ssid = parts.next().unwrap_or_default();
        let signal = parts.next().and_then(|s| s.parse::<u8>().ok());
        return Some(((!ssid.is_empty()).then(|| ssid.to_string()), signal));
    }
    None
}

/// The `SSID : xxx` line of `ipconfig getsummary` / `netsh wlan show
/// interfaces` output (both keep the SSID label in English). BSSID lines
/// don't match: they start with 'B'.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn parse_ssid_field(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim_start)
        .find(|l| l.starts_with("SSID") && l.contains(':'))
        .map(|l| l.split_once(':').unwrap().1.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The `Signal : 88%` line of netsh output. The label is localized ("信号");
/// the "NN%" value tail is not.
#[cfg(any(target_os = "windows", test))]
fn parse_signal_field(text: &str) -> Option<u8> {
    text.lines().find_map(|l| {
        let (_, pct) = l.split_once(':')?;
        let pct = pct.trim().strip_suffix('%')?;
        pct.parse::<u8>().ok().map(|v| v.min(100))
    })
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn ssid_field_parses_netsh_and_ipconfig_shapes() {
        let netsh = "    名称                   : WLAN\n    \
                      SSID                   : MyHome\n    \
                      BSSID                  : aa:bb:cc:dd:ee:ff\n";
        assert_eq!(super::parse_ssid_field(netsh).as_deref(), Some("MyHome"));
        let ipconfig = "uptime = 1234\n SSID : <redacted>\n BSSID : ff:ee:dd\n";
        assert_eq!(super::parse_ssid_field(ipconfig).as_deref(), Some("<redacted>"));
        assert_eq!(super::parse_ssid_field("no ssid here"), None);
        assert_eq!(super::parse_ssid_field(" SSID :\n"), None);
    }

    #[test]
    fn signal_field_handles_localized_labels() {
        let netsh = "    信号                   : 88%\n    \
                      接收速率 (Mbps)         : 866.7\n";
        assert_eq!(super::parse_signal_field(netsh), Some(88));
        assert_eq!(super::parse_signal_field("no percent lines"), None);
        assert_eq!(super::parse_signal_field("信号                   : 120%"), Some(100));
    }

    #[test]
    fn top_processes_is_valid_json() {
        let v: serde_json::Value = serde_json::from_str(&super::top_processes().to_string())
            .expect("valid json");
        assert!(v["cpu_percent"].is_f64());
        assert!(v["memory_total_gb"].as_f64().unwrap() > 0.0);
        assert!(v["top_by_cpu"].as_array().unwrap().len() <= 5);
    }

    #[test]
    fn storage_is_valid_json() {
        let v: serde_json::Value =
            serde_json::from_str(&super::storage().to_string()).expect("valid json");
        let disks = v["disks"].as_array().expect("disks array");
        assert!(!disks.is_empty(), "at least one disk");
        for d in disks {
            assert!(d["total_gb"].as_f64().unwrap() >= 1.0);
        }
    }

    #[test]
    fn network_status_is_valid_json() {
        let v: serde_json::Value =
            serde_json::from_str(&super::network_status().to_string()).expect("valid json");
        assert_eq!(v["online"], true);
        assert!(v["ip"].is_string(), "local ip present");
    }
}
