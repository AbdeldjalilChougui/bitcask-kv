//! On-disk record formats.
//!
//! Data record:  `crc32 u32 | key_len u32 | val_len u32 | key | value`
//! (little endian; `val_len == u32::MAX` marks a tombstone with no value).
//! The CRC covers everything after itself.
//!
//! Hint record:  `crc32 u32 | key_len u32 | rec_len u32 | rec_pos u64 | key`
//! pointing at the matching record in the data file with the same id.

pub const HEADER: usize = 12;
pub const TOMBSTONE: u32 = u32::MAX;
pub const HINT_HEADER: usize = 20;

pub fn encode(key: &[u8], value: Option<&[u8]>) -> Vec<u8> {
    let vlen = value.map_or(TOMBSTONE, |v| v.len() as u32);
    let mut buf = Vec::with_capacity(HEADER + key.len() + value.map_or(0, <[u8]>::len));
    buf.extend_from_slice(&[0; 4]);
    buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
    buf.extend_from_slice(&vlen.to_le_bytes());
    buf.extend_from_slice(key);
    if let Some(v) = value {
        buf.extend_from_slice(v);
    }
    let crc = crc32fast::hash(&buf[4..]);
    buf[..4].copy_from_slice(&crc.to_le_bytes());
    buf
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// A decoded record borrowed from a buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct Decoded<'a> {
    pub key: &'a [u8],
    /// `None` for a tombstone.
    pub value: Option<&'a [u8]>,
    pub len: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Not enough bytes: a torn write at the end of a file.
    Truncated,
    /// Checksum mismatch: corruption.
    Corrupt,
}

/// Total record length implied by a header, if the header is present.
pub fn record_len(buf: &[u8]) -> Result<usize, DecodeError> {
    if buf.len() < HEADER {
        return Err(DecodeError::Truncated);
    }
    let klen = u32_at(buf, 4) as usize;
    let vlen = u32_at(buf, 8);
    let vlen = if vlen == TOMBSTONE { 0 } else { vlen as usize };
    Ok(HEADER + klen + vlen)
}

pub fn decode(buf: &[u8]) -> Result<Decoded<'_>, DecodeError> {
    let len = record_len(buf)?;
    if buf.len() < len {
        return Err(DecodeError::Truncated);
    }
    if crc32fast::hash(&buf[4..len]) != u32_at(buf, 0) {
        return Err(DecodeError::Corrupt);
    }
    let klen = u32_at(buf, 4) as usize;
    let key = &buf[HEADER..HEADER + klen];
    let value = if u32_at(buf, 8) == TOMBSTONE { None } else { Some(&buf[HEADER + klen..len]) };
    Ok(Decoded { key, value, len })
}

pub fn encode_hint(key: &[u8], rec_len: u32, rec_pos: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HINT_HEADER + key.len());
    buf.extend_from_slice(&[0; 4]);
    buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
    buf.extend_from_slice(&rec_len.to_le_bytes());
    buf.extend_from_slice(&rec_pos.to_le_bytes());
    buf.extend_from_slice(key);
    let crc = crc32fast::hash(&buf[4..]);
    buf[..4].copy_from_slice(&crc.to_le_bytes());
    buf
}

/// Decode one hint record: `(key, rec_len, rec_pos, bytes used)`.
pub fn decode_hint(buf: &[u8]) -> Result<(&[u8], u32, u64, usize), DecodeError> {
    if buf.len() < HINT_HEADER {
        return Err(DecodeError::Truncated);
    }
    let klen = u32_at(buf, 4) as usize;
    let len = HINT_HEADER + klen;
    if buf.len() < len {
        return Err(DecodeError::Truncated);
    }
    if crc32fast::hash(&buf[4..len]) != u32_at(buf, 0) {
        return Err(DecodeError::Corrupt);
    }
    let rec_len = u32_at(buf, 8);
    let rec_pos = u64::from_le_bytes(buf[12..20].try_into().unwrap());
    Ok((&buf[HINT_HEADER..len], rec_len, rec_pos, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_value_and_tombstone() {
        let r = encode(b"key", Some(b"value"));
        let d = decode(&r).unwrap();
        assert_eq!((d.key, d.value, d.len), (&b"key"[..], Some(&b"value"[..]), r.len()));
        let t = encode(b"key", None);
        assert_eq!(decode(&t).unwrap().value, None);
        assert_eq!(decode(&encode(b"k", Some(b""))).unwrap().value, Some(&b""[..]));
    }

    #[test]
    fn detects_truncation_and_corruption() {
        let r = encode(b"key", Some(b"value"));
        for i in 0..r.len() {
            assert_eq!(decode(&r[..i]), Err(DecodeError::Truncated));
        }
        let mut bad = r.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(decode(&bad), Err(DecodeError::Corrupt));
    }

    #[test]
    fn hint_roundtrip() {
        let h = encode_hint(b"abc", 42, 1 << 40);
        assert_eq!(decode_hint(&h).unwrap(), (&b"abc"[..], 42, 1 << 40, h.len()));
        assert_eq!(decode_hint(&h[..h.len() - 1]), Err(DecodeError::Truncated));
    }
}
