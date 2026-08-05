//! DMAP (Digital Media Access Protocol) binary encoding.
//!
//! Wire format is Type-Length-Value:
//!   [4-byte ASCII tag][4-byte big-endian length][value bytes]
//!
//! Containers hold nested DMAP elements as their value. See
//! `owntone/src/dmap_fields.gperf` for the full reversed tag table.

use bytes::{BufMut, BytesMut};

/// A 4-char ASCII DMAP tag, e.g. `msrv`, `mlit`, `asar`.
pub type Tag = [u8; 4];

pub fn tag(s: &str) -> Tag {
    let b = s.as_bytes();
    assert_eq!(b.len(), 4, "DMAP tags must be exactly 4 ASCII bytes");
    [b[0], b[1], b[2], b[3]]
}

fn write_header(out: &mut BytesMut, tag: Tag, len: u32) {
    out.put_slice(&tag);
    out.put_u32(len);
}

pub fn u8_field(out: &mut BytesMut, t: Tag, v: u8) {
    write_header(out, t, 1);
    out.put_u8(v);
}

pub fn u16_field(out: &mut BytesMut, t: Tag, v: u16) {
    write_header(out, t, 2);
    out.put_u16(v);
}

pub fn u32_field(out: &mut BytesMut, t: Tag, v: u32) {
    write_header(out, t, 4);
    out.put_u32(v);
}

pub fn u64_field(out: &mut BytesMut, t: Tag, v: u64) {
    write_header(out, t, 8);
    out.put_u64(v);
}

pub fn string_field(out: &mut BytesMut, t: Tag, v: &str) {
    let b = v.as_bytes();
    write_header(out, t, b.len() as u32);
    out.put_slice(b);
}

/// Emit a string field where the value bytes have already been encoded to
/// the wire charset (e.g. via `charset::to_macroman`).
pub fn string_field_bytes(out: &mut BytesMut, t: Tag, v: &[u8]) {
    write_header(out, t, v.len() as u32);
    out.put_slice(v);
}

/// Build a container: run `body` to fill nested fields, then wrap them with `t`.
pub fn container(out: &mut BytesMut, t: Tag, body: impl FnOnce(&mut BytesMut)) {
    let start = out.len();
    // Reserve 8 bytes for header, backfill length once body is written.
    write_header(out, t, 0);
    body(out);
    let body_len = (out.len() - start - 8) as u32;
    out[start + 4..start + 8].copy_from_slice(&body_len.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_u32_field() {
        let mut buf = BytesMut::new();
        u32_field(&mut buf, tag("miid"), 42);
        assert_eq!(&buf[..], &[b'm', b'i', b'i', b'd', 0, 0, 0, 4, 0, 0, 0, 42]);
    }

    #[test]
    fn encodes_string_field() {
        let mut buf = BytesMut::new();
        string_field(&mut buf, tag("minm"), "Hello");
        assert_eq!(
            &buf[..],
            &[b'm', b'i', b'n', b'm', 0, 0, 0, 5, b'H', b'e', b'l', b'l', b'o']
        );
    }

    #[test]
    fn container_backfills_length() {
        let mut buf = BytesMut::new();
        container(&mut buf, tag("mlit"), |b| {
            u32_field(b, tag("miid"), 1);
            string_field(b, tag("minm"), "x");
        });
        // header 8 + miid(12) + minm(9) = 29 total; body length = 21
        assert_eq!(&buf[0..4], b"mlit");
        assert_eq!(&buf[4..8], &21u32.to_be_bytes());
        assert_eq!(buf.len(), 29);
    }
}
