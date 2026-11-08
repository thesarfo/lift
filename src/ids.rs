//! ObjectId -> UUID. The 12 ObjectId bytes, then a fixed 4-byte suffix, so the original id can
//! always be read back out of the UUID. Shepherd uses the suffix "SHEP" (53484550); the API side
//! lives in shepherd-api-dotnet `Ids/ObjectIdGuid.cs` and must use the same bytes.

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
        let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
        format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
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

#[cfg(test)]
mod tests {
    use super::*;

    fn shep() -> Suffix {
        Suffix::parse("53484550").unwrap()
    }

    // Same vector as ObjectIdGuidTests.ObjectId_MapsToGuidWithShepSuffix in shepherd-api-dotnet.
    #[test]
    fn matches_the_api_layout() {
        let oid = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        assert_eq!(shep().uuid(&oid), "507f1f77-bcf8-6cd7-9943-901153484550");
    }

    #[test]
    fn zero_object_id_is_nil() {
        assert_eq!(shep().uuid(&ObjectId::from_bytes([0; 12])), NIL);
    }

    #[test]
    fn string_ids() {
        let s = shep();
        assert_eq!(s.uuid_from_str("507f1f77bcf86cd799439011").unwrap(), "507f1f77-bcf8-6cd7-9943-901153484550");
        assert_eq!(s.uuid_from_str("10000000-0000-0000-0000-000000000001").unwrap(), "10000000-0000-0000-0000-000000000001");
        assert_eq!(s.uuid_from_str("Ghana"), None);
    }

    #[test]
    fn bad_suffix() {
        assert!(Suffix::parse("534845").is_err());
        assert!(Suffix::parse("zz484550").is_err());
    }
}
