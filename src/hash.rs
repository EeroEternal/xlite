use rand::RngCore;
use sha2::{Digest, Sha256};

pub fn generate_api_key() -> String {
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    let mut out = String::from("sk-");
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub fn hash_api_key(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    // Hot path (every request): manual hex encode avoids `format!`'s fmt
    // machinery on the way to the snapshot lookup.
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub fn mask_api_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 8 {
        return "********".to_string();
    }
    let prefix_len = if len > 12 { 6 } else { 3 };
    let suffix_len = if len > 12 { 4 } else { 3 };
    format!("{}...{}", &key[..prefix_len], &key[len - suffix_len..])
}

pub fn key_prefix(key: &str) -> String {
    if key.len() >= 8 {
        key[..8].to_string()
    } else {
        key.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_sha256_hex() {
        assert_eq!(hash_api_key("sk-test").len(), 64);
        assert_eq!(hash_api_key("sk-test"), hash_api_key("sk-test"));
        assert_ne!(hash_api_key("sk-test"), hash_api_key("sk-other"));
    }

    #[test]
    fn mask_keeps_prefix_and_suffix() {
        let masked = mask_api_key("sk-abcdefghijklmnop");
        assert!(masked.starts_with("sk-abc"));
        assert!(masked.ends_with("mnop"));
        assert!(masked.contains("..."));
    }
}
