//! Stable per-machine identity, mirroring the ESP32 firmware:
//! device_id = MAC address ("aa:bb:cc:dd:ee:ff", SystemInfo::GetMacAddress),
//! client_id = persistent UUIDv4 (Board::GenerateUuid, random once, kept in
//! NVS forever). A desktop has no one-shot NVS, so the UUIDv4 bits are derived
//! from a hash of local hardware info — deterministic per machine unless
//! overridden in the config file.

use mac_address::get_mac_address;

/// First non-loopback MAC, formatted like the firmware's MAC string.
fn local_mac() -> Option<String> {
    // get_mac_address iterates interfaces; prefer the first real one.
    // (On most machines all physical NICs share the same hardware addresses
    // family; any stable one is fine for identity purposes.)
    get_mac_address().ok().flatten().map(|m| {
        let b = m.bytes();
        b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":")
    })
}

/// Fallback seed when no MAC is available (rare; e.g. containers).
fn fallback_seed() -> String {
    // HOSTNAME/USER on Unix, COMPUTERNAME/USERNAME on Windows, and the
    // hostname file as a last resort (Linux containers without env vars).
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| std::fs::read_to_string("/etc/hostname").unwrap_or_default());
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    format!("{host}|{user}")
}

fn seed() -> String {
    local_mac().unwrap_or_else(fallback_seed)
}

/// device_id: the MAC itself, matching the device firmware's Device-Id.
pub fn device_id() -> String {
    seed()
}

/// client_id: UUIDv4-formatted hash of the machine seed. Bits 6/8 are set to
/// version 4 / variant 1 exactly like Board::GenerateUuid.
pub fn client_id() -> String {
    // FxHash-ish mix of sha-like rounds; std has no sha2, use a simple but
    // well-distributed 128-bit mix of the seed (identity, not a secret).
    let bytes = seed().into_bytes();
    let mut h1: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    let mut h2: u64 = 0x9e3779b97f4a7c15; // golden ratio
    for &b in &bytes {
        h1 ^= b as u64;
        h1 = h1.wrapping_mul(0x100000001b3); // FNV prime
        h2 = h2.rotate_left(7) ^ (h2.wrapping_mul(0x2545F4914F6CDD1D) ^ b as u64);
    }
    // widen to 16 bytes
    let mut v = [0u8; 16];
    v[..8].copy_from_slice(&h1.to_le_bytes());
    v[8..].copy_from_slice(&h2.wrapping_mul(0xff51afd7ed558ccd).rotate_left(31).to_le_bytes());
    v[6] = (v[6] & 0x0f) | 0x40; // version 4
    v[8] = (v[8] & 0x3f) | 0x80; // variant 1
    let hex: Vec<String> = v.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_across_calls() {
        let d1 = device_id();
        let d2 = device_id();
        let c1 = client_id();
        let c2 = client_id();
        assert_eq!(d1, d2, "device_id must be stable");
        assert_eq!(c1, c2, "client_id must be stable");
    }

    #[test]
    fn device_id_is_mac_format() {
        let d = device_id();
        assert_eq!(d.len(), 17, "mac string length: {d}");
        assert_eq!(d.split(':').count(), 6, "mac colon count: {d}");
        assert!(d.bytes().all(|b| b.is_ascii_hexdigit() || b == b':'));
    }

    #[test]
    fn client_id_is_uuid_v4() {
        let c = client_id();
        assert_eq!(c.len(), 36);
        assert_eq!(c.split('-').map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        let v = c.split('-').nth(2).unwrap().chars().next().unwrap();
        assert_eq!(v, '4', "version nibble: {c}");
    }
}
