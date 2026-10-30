use std::path::PathBuf;

use crate::Response;

fn dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("meersh").join("llm"))
}

/// FNV-1a over the request parts. Stable across builds, unlike `DefaultHasher`.
pub fn key(parts: &[&str]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.bytes().chain([0xff]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

pub fn get(key: &str) -> Option<Response> {
    let text = std::fs::read_to_string(dir()?.join(format!("{key}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

/// Best effort. A shell must not fail because its cache directory is read only.
pub fn put(key: &str, response: &Response) {
    let Some(dir) = dir() else { return };
    if std::fs::create_dir_all(&dir).is_ok()
        && let Ok(text) = serde_json::to_string(response)
    {
        let _ = std::fs::write(dir.join(format!("{key}.json")), text);
    }
}
