//! ObjectId -> UUID. The 12 ObjectId bytes, then a fixed 4-byte suffix, so the original id can
//! always be read back out of the UUID. An app that needs to accept old ObjectId strings must use
//! the same suffix as the mapping's `id_suffix`.

use anyhow::{Context, Result, bail};
use bson::oid::ObjectId;

pub const NIL: &str = "00000000-0000-0000-0000-000000000000";

#[derive(Clone, Copy, Debug)]
pub struct Suffix([u8; 4]);

impl Suffix {
    pub fn parse(hex: &str) -> Result<Self> {
        if hex.len() != 8 {
            bail!("id_suffix must be 8 hex chars, got {hex:?}");
        }
        let mut b = [0u8; 4];
        for (i, byte) in b.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).context("id_suffix is not hex")?;
        }
        Ok(Self(b))
    }

    /// Go's zero ObjectId means "unset", which the API reads as Guid.Empty.
    pub fn uuid(&self, oid: &ObjectId) -> String {
        let o = oid.bytes();
        if o == [0; 12] {
            return NIL.into();
        }
        let mut b = [0u8; 16];
        b[..12].copy_from_slice(&o);
        b[12..].copy_from_slice(&self.0);
        uuid_string(&b)
    }

    /// A string id from a document: 24-char hex becomes our UUID, a UUID passes through.
    pub fn uuid_from_str(&self, s: &str) -> Option<String> {
        if s.len() == 24 {
            return ObjectId::parse_str(s).ok().map(|o| self.uuid(&o));
        }
        let plain: String = s.chars().filter(|c| *c != '-').collect();
        (plain.len() == 32 && plain.chars().all(|c| c.is_ascii_hexdigit())).then(|| s.to_lowercase())
    }
}

/// 16 bytes as a lowercase hyphenated UUID, without per-byte allocations (this runs for every id).
pub fn uuid_string(b: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 15)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suffix() -> Suffix {
        Suffix::parse("6c696674").unwrap()
    }

    #[test]
    fn object_id_then_suffix() {
        let oid = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        assert_eq!(suffix().uuid(&oid), "507f1f77-bcf8-6cd7-9943-90116c696674");
    }

    #[test]
    fn zero_object_id_is_nil() {
        assert_eq!(suffix().uuid(&ObjectId::from_bytes([0; 12])), NIL);
    }

    #[test]
    fn string_ids() {
        let s = suffix();
        assert_eq!(s.uuid_from_str("507f1f77bcf86cd799439011").unwrap(), "507f1f77-bcf8-6cd7-9943-90116c696674");
        assert_eq!(s.uuid_from_str("10000000-0000-0000-0000-000000000001").unwrap(), "10000000-0000-0000-0000-000000000001");
        assert_eq!(s.uuid_from_str("not-an-id"), None);
    }

    #[test]
    fn bad_suffix() {
        assert!(Suffix::parse("534845").is_err());
        assert!(Suffix::parse("zz484550").is_err());
    }
}
