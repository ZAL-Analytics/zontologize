use sha2::{Digest, Sha256};

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn stage_key(stage: &str, version: u32, parts: &[&str]) -> String {
    let mut key = format!("{stage}-v{version}");
    for part in parts {
        key.push(' ');
        key.push_str(part);
    }
    key
}

pub fn stage_hash(tag: &[&[u8]], version: u32, body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    for part in tag {
        hasher.update(part);
    }
    hasher.update(version.to_le_bytes());
    hasher.update(body);
    hex(&hasher.finalize())
}
