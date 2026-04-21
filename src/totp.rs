use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha1 = Hmac<Sha1>;

pub fn generate_secret_b32() -> String {
    let mut bytes = [0u8; 20];
    getrandom::getrandom(&mut bytes).expect("getrandom");
    base32::encode(base32::Alphabet::Rfc4648 { padding: false }, &bytes)
}

fn decode_b32(secret_b32: &str) -> Option<Vec<u8>> {
    base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret_b32)
        .or_else(|| base32::decode(base32::Alphabet::Rfc4648 { padding: true }, secret_b32))
}

pub fn code_at(secret: &[u8], step: u64) -> String {
    let mut mac = HmacSha1::new_from_slice(secret).expect("hmac key");
    mac.update(&step.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let offset = (hash[19] & 0x0f) as usize;
    let code = u32::from_be_bytes([
        hash[offset] & 0x7f,
        hash[offset + 1],
        hash[offset + 2],
        hash[offset + 3],
    ]) % 1_000_000;
    format!("{:06}", code)
}

pub fn verify(secret_b32: &str, submitted: &str) -> bool {
    let clean: String = submitted.chars().filter(|c| c.is_ascii_digit()).collect();
    if clean.len() != 6 {
        return false;
    }
    let Some(secret) = decode_b32(secret_b32) else {
        return false;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return false;
    };
    let step = now.as_secs() / 30;
    for delta in [0i64, -1, 1] {
        let s = (step as i64 + delta).max(0) as u64;
        if constant_time_eq(code_at(&secret, s).as_bytes(), clean.as_bytes()) {
            return true;
        }
    }
    false
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut r = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        r |= x ^ y;
    }
    r == 0
}

pub fn otpauth_url(secret_b32: &str, account: &str, issuer: &str) -> String {
    let issuer_enc = url_encode(issuer);
    let account_enc = url_encode(account);
    format!(
        "otpauth://totp/{issuer_enc}:{account_enc}?secret={secret_b32}&issuer={issuer_enc}&algorithm=SHA1&digits=6&period=30"
    )
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}
