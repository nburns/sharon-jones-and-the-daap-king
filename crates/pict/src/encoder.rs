//! PICT Version-2 emitter for the two shapes we care about:
//!   * indexed 8-bit color via `PackBitsRect` (0x0098)
//!   * 1-bit bitmap via `BitsRect` (0x0090)
//!
//! Structure of the file we emit:
//!   * 512-byte header pad (traditional; readers ignore it)
//!   * Picture size — 2 bytes, u16 (obsolete for v2 but must be written)
//!   * Picture frame — 8 bytes (top, left, bottom, right)
//!   * VersionOp (0x0011) + Version 2 (0x02FF)
//!   * HeaderOp (0x0C00) + 24-byte header record with resolution + bounds
//!   * DefHilite (0x001E) — optional, safe to include
//!   * Clip (0x0001) + region setting the clip rect
//!   * BitsRect / PackBitsRect + payload
//!   * EndOfPicture (0x00FF)
//!
//! All multi-byte fields are big-endian. Even-alignment is required after
//! opcodes in version 2 — we align by advancing to the next even byte
//! offset before each opcode.
//!
//! Reference: Inside Macintosh: Imaging with QuickDraw, chapter 7 "Pictures".

use crate::palette::Rgb;
use crate::PictError;

const OP_NOP: u16 = 0x0000;
const OP_CLIP: u16 = 0x0001;
const OP_DEF_HILITE: u16 = 0x001E;
const OP_VERSION_OP: u16 = 0x0011;
const OP_HEADER_OP: u16 = 0x0C00;
const OP_BITS_RECT: u16 = 0x0090;
const OP_PACK_BITS_RECT: u16 = 0x0098;
const OP_END_OF_PICTURE: u16 = 0x00FF;
const VERSION_2: u16 = 0x02FF;

/// Encode a full-color indexed PICT. `indices` is width×height u8 palette
/// indices, `palette` is the CLUT they point into.
pub fn encode_indexed(
    width: u32,
    height: u32,
    palette: &[Rgb; 256],
    indices: &[u8],
) -> Result<Vec<u8>, PictError> {
    validate_dims(width, height)?;
    let expected = (width * height) as usize;
    if indices.len() != expected {
        return Err(PictError::LenMismatch {
            got: indices.len(),
            expected,
        });
    }

    let mut out = Vec::with_capacity(1024 + expected);
    write_common_prelude(&mut out, width, height);

    // PackBitsRect opcode
    align_even(&mut out);
    write_u16(&mut out, OP_PACK_BITS_RECT);
    write_pixmap_header(&mut out, width, height, /*pixel_size=*/ 8);
    write_color_table(&mut out, palette);
    write_srcrect(&mut out, width, height); // srcRect
    write_srcrect(&mut out, width, height); // dstRect
    write_u16(&mut out, 0); // mode = srcCopy

    // Pixel data — PackBits-compressed rows.
    // rowBytes computed by write_pixmap_header is (width) rounded up to
    // even, since pixel_size=8 means 1 byte per pixel; here we recompute.
    let row_bytes = pixmap_row_bytes(width, 8);
    let mut row_buf = vec![0u8; row_bytes as usize];
    for y in 0..height as usize {
        let row_start = y * width as usize;
        row_buf[..width as usize]
            .copy_from_slice(&indices[row_start..row_start + width as usize]);
        // Padding bytes (if width was odd) get left as zero.
        let packed = pack_bits(&row_buf[..row_bytes as usize]);
        // rowBytes ≥ 8 → 2-byte length; else 1 byte. Standard.
        if row_bytes > 250 {
            write_u16(&mut out, packed.len() as u16);
        } else {
            out.push(packed.len() as u8);
        }
        out.extend_from_slice(&packed);
    }

    align_even(&mut out);
    write_u16(&mut out, OP_END_OF_PICTURE);
    Ok(out)
}

/// Encode a 1-bit bitmap PICT. `bits` is MSB-first row-major with row
/// stride = `(width + 7) / 8` (must match `dither::atkinson_1bit` output).
pub fn encode_bitmap(width: u32, height: u32, bits: &[u8]) -> Result<Vec<u8>, PictError> {
    validate_dims(width, height)?;
    let expected_row = width.div_ceil(8) as usize;
    let expected = expected_row * height as usize;
    if bits.len() != expected {
        return Err(PictError::LenMismatch {
            got: bits.len(),
            expected,
        });
    }

    let mut out = Vec::with_capacity(512 + expected);
    write_common_prelude(&mut out, width, height);

    align_even(&mut out);
    write_u16(&mut out, OP_BITS_RECT);
    // BitMap struct: baseAddr (not stored in PICT), rowBytes u16, bounds
    // Rect, plus pixel bits. rowBytes for a BitMap is (width+15)/16 * 2 —
    // must be even and MSB of the u16 must be 0 (distinguishes from a
    // PixMap where the MSB is 1).
    let row_bytes = bitmap_row_bytes(width);
    write_u16(&mut out, row_bytes);
    // bounds Rect (top, left, bottom, right)
    write_rect(&mut out, 0, 0, height as i16, width as i16);
    // srcRect
    write_rect(&mut out, 0, 0, height as i16, width as i16);
    // dstRect
    write_rect(&mut out, 0, 0, height as i16, width as i16);
    // mode = srcCopy
    write_u16(&mut out, 0);

    // Pixel data — one row at a time, uncompressed for BitsRect (opcode
    // 0x0090 is the uncompressed variant; PackBitsRect at 0x0098 would
    // pack). BitsRect is fine at our sizes.
    let src_stride = expected_row;
    for y in 0..height as usize {
        let src_row = &bits[y * src_stride..(y + 1) * src_stride];
        out.extend_from_slice(src_row);
        // Pad row to rowBytes if wider (rowBytes is always even, src_stride
        // ceils to nearest byte).
        for _ in src_stride..row_bytes as usize {
            out.push(0);
        }
    }

    align_even(&mut out);
    write_u16(&mut out, OP_END_OF_PICTURE);
    Ok(out)
}

fn validate_dims(width: u32, height: u32) -> Result<(), PictError> {
    if width == 0 || height == 0 {
        return Err(PictError::ZeroDim);
    }
    if width > 32767 {
        return Err(PictError::WidthTooLarge(width));
    }
    if height > 32767 {
        return Err(PictError::HeightTooLarge(height));
    }
    Ok(())
}

fn write_common_prelude(out: &mut Vec<u8>, width: u32, height: u32) {
    // 512-byte header pad
    out.extend_from_slice(&[0u8; 512]);
    // Picture size (obsolete in v2 but placeholder written); leave zero.
    write_u16(out, 0);
    // Picture frame: top, left, bottom, right
    write_rect(out, 0, 0, height as i16, width as i16);
    // VersionOp + Version 2
    write_u16(out, OP_VERSION_OP);
    write_u16(out, VERSION_2);
    // HeaderOp (0x0C00) + 24-byte header record.
    write_u16(out, OP_HEADER_OP);
    // -1_i16 signals extended header; then bogus fields ok for v2.
    write_i16(out, -1);
    write_u16(out, 0); // reserved
    // hResolution / vResolution as 32-bit fixed-point (72.0 dpi = 0x0048_0000)
    write_u32(out, 0x0048_0000);
    write_u32(out, 0x0048_0000);
    // srcRect (top, left, bottom, right) fixed-point in v2 header
    write_rect(out, 0, 0, height as i16, width as i16);
    write_u32(out, 0); // reserved
    // Optional DefHilite — cosmetic
    write_u16(out, OP_DEF_HILITE);
    // Clip region: opcode already written per-caller (well, we write it
    // here so both encoders inherit the same clip).
    write_u16(out, OP_CLIP);
    // Clip region size = 10 (u16 size + 8-byte rect)
    write_u16(out, 10);
    write_rect(out, 0, 0, height as i16, width as i16);
    // NOP for even alignment before the caller's real opcode
    let _ = OP_NOP;
    align_even(out);
}

fn write_pixmap_header(out: &mut Vec<u8>, width: u32, height: u32, pixel_size: u16) {
    // rowBytes with MSB set (indicates PixMap, not BitMap)
    let rb = pixmap_row_bytes(width, pixel_size);
    // High bit set to signal PixMap.
    write_u16(out, rb | 0x8000);
    // bounds
    write_rect(out, 0, 0, height as i16, width as i16);
    // pmVersion
    write_u16(out, 0);
    // packType (0 = default for our data; PackBits per row)
    write_u16(out, 0);
    // packSize
    write_u32(out, 0);
    // hRes, vRes (72 dpi fixed-point)
    write_u32(out, 0x0048_0000);
    write_u32(out, 0x0048_0000);
    // pixelType (0 = chunky/indexed)
    write_u16(out, 0);
    // pixelSize
    write_u16(out, pixel_size);
    // cmpCount
    write_u16(out, 1);
    // cmpSize
    write_u16(out, pixel_size);
    // planeBytes
    write_u32(out, 0);
    // pmTable handle (unused; write zero)
    write_u32(out, 0);
    // pmReserved
    write_u32(out, 0);
}

fn write_color_table(out: &mut Vec<u8>, palette: &[Rgb; 256]) {
    // ctSeed (uniquifier)
    write_u32(out, 0);
    // ctFlags
    write_u16(out, 0);
    // ctSize = numEntries - 1
    write_u16(out, 255);
    for (i, c) in palette.iter().enumerate() {
        write_u16(out, i as u16); // pixel value
        // Each channel is written as a 16-bit value: replicate 8-bit to
        // both bytes (0xRR → 0xRRRR).
        write_u16(out, ((c.r as u16) << 8) | c.r as u16);
        write_u16(out, ((c.g as u16) << 8) | c.g as u16);
        write_u16(out, ((c.b as u16) << 8) | c.b as u16);
    }
}

fn write_srcrect(out: &mut Vec<u8>, width: u32, height: u32) {
    write_rect(out, 0, 0, height as i16, width as i16);
}

fn write_rect(out: &mut Vec<u8>, top: i16, left: i16, bottom: i16, right: i16) {
    write_i16(out, top);
    write_i16(out, left);
    write_i16(out, bottom);
    write_i16(out, right);
}

fn write_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn write_i16(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn align_even(out: &mut Vec<u8>) {
    if out.len() & 1 == 1 {
        out.push(0);
    }
}

fn pixmap_row_bytes(width: u32, pixel_size: u16) -> u16 {
    // rowBytes = ((width * pixelSize + 15) / 16) * 2 — always even.
    let bits = width as u64 * pixel_size as u64;
    let bytes = ((bits + 15) / 16) * 2;
    bytes as u16
}

fn bitmap_row_bytes(width: u32) -> u16 {
    // Same alignment as PixMap: nearest multiple of 2 bytes; MSB must be 0.
    let bytes = (width as u64).div_ceil(16) * 2;
    // Cap so MSB stays clear (rowBytes < 0x8000).
    (bytes as u16) & 0x7FFF
}

/// Apple PackBits byte RLE. Runs of ≥3 equal bytes become (0x100-run_len)
/// + byte; other spans become (literal_len - 1) + bytes. Output is per
/// row and never bigger than `input_len + input_len/128 + 1`.
fn pack_bits(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 128 + 1);
    let mut i = 0;
    while i < input.len() {
        // Look-ahead for a run.
        let mut run_len = 1;
        while i + run_len < input.len()
            && input[i + run_len] == input[i]
            && run_len < 128
        {
            run_len += 1;
        }
        if run_len >= 3 {
            // Encode as a run.
            out.push(((0x100 - run_len as u16) & 0xFF) as u8);
            out.push(input[i]);
            i += run_len;
        } else {
            // Encode as a literal, scanning ahead to grow the literal
            // until we hit a real run or the max length (128).
            let start = i;
            let mut lit_len = 1;
            i += 1;
            while i < input.len() && lit_len < 128 {
                // Peek ahead — if there's a 3-byte run starting here,
                // stop the literal so the run gets its own encoding.
                let looks_like_run = i + 2 < input.len()
                    && input[i] == input[i + 1]
                    && input[i] == input[i + 2];
                if looks_like_run {
                    break;
                }
                lit_len += 1;
                i += 1;
            }
            out.push((lit_len - 1) as u8);
            out.extend_from_slice(&input[start..start + lit_len]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::MAC_SYSTEM_PALETTE;

    #[test]
    fn packbits_run_of_five() {
        // 5 identical bytes → run header 0xFC (=256-4?) actually 256-5 = 0xFB.
        let out = pack_bits(&[0xAA, 0xAA, 0xAA, 0xAA, 0xAA]);
        assert_eq!(out, vec![0xFB, 0xAA]);
    }

    #[test]
    fn packbits_literal_three_distinct() {
        let out = pack_bits(&[1, 2, 3]);
        assert_eq!(out, vec![0x02, 1, 2, 3]);
    }

    #[test]
    fn packbits_mixed_literal_then_run() {
        // literal [1,2] (header = len-1 = 1) then run of 4 threes
        // (header = 256 - 4 = 252 = 0xFC), then the byte 3
        let out = pack_bits(&[1, 2, 3, 3, 3, 3]);
        assert_eq!(out, vec![0x01, 1, 2, 0xFC, 3]);
    }

    #[test]
    fn encode_indexed_smoke_test() {
        let indices = vec![0u8; 4 * 4];
        let bytes = encode_indexed(4, 4, &MAC_SYSTEM_PALETTE, &indices).unwrap();
        // 512 header pad + a few opcodes + palette + payload — verify at
        // least the version signature is where it should be.
        assert!(bytes.len() > 512 + 20);
        // Version-2 sentinel (0x02FF) appears after 512-byte pad + 2 (size)
        // + 8 (frame Rect) + 2 (VersionOp).
        let off = 512 + 2 + 8 + 2;
        assert_eq!(&bytes[off..off + 2], &[0x02, 0xFF]);
    }

    #[test]
    fn encode_bitmap_smoke_test() {
        // 16px wide × 4 rows, all bits = 0
        let bits = vec![0u8; 2 * 4];
        let bytes = encode_bitmap(16, 4, &bits).unwrap();
        assert!(bytes.len() > 512 + 20);
    }

    #[test]
    fn encode_indexed_len_mismatch_errors() {
        let e = encode_indexed(4, 4, &MAC_SYSTEM_PALETTE, &vec![0u8; 3]).unwrap_err();
        assert!(matches!(e, PictError::LenMismatch { got: 3, expected: 16 }));
    }

    #[test]
    fn encode_bitmap_zero_dims_errors() {
        let e = encode_bitmap(0, 4, &[]).unwrap_err();
        assert!(matches!(e, PictError::ZeroDim));
    }

    #[test]
    fn pixmap_row_bytes_is_even() {
        assert_eq!(pixmap_row_bytes(1, 8), 2);
        assert_eq!(pixmap_row_bytes(2, 8), 2);
        assert_eq!(pixmap_row_bytes(3, 8), 4);
        assert_eq!(pixmap_row_bytes(200, 8), 200);
        assert_eq!(pixmap_row_bytes(201, 8), 202);
    }

    #[test]
    fn bitmap_row_bytes_matches_reference() {
        assert_eq!(bitmap_row_bytes(1), 2);
        assert_eq!(bitmap_row_bytes(8), 2);
        assert_eq!(bitmap_row_bytes(9), 2);
        assert_eq!(bitmap_row_bytes(16), 2);
        assert_eq!(bitmap_row_bytes(17), 4);
    }
}
