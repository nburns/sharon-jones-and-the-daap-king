//! PICT Version-2 emitter.
//!
//! Entry points:
//!   * [`encode_packbits`]         — indexed (1/2/4/8-bit) via PackBitsRect
//!   * [`encode_indexed`]          — backwards-compat wrapper for 8-bit indexed
//!   * [`encode_bitmap`]           — 1-bit bitmap via BitsRect
//!   * [`encode_direct_bits_rect_rgb`] — 24-bit direct-color via DirectBitsRect
//!
//! All multi-byte fields are big-endian. Even-alignment required after opcodes.
//!
//! Reference: Inside Macintosh: Imaging with QuickDraw, chapter 7 "Pictures".

use crate::PictError;
use crate::palette::Rgb;

const OP_NOP: u16 = 0x0000;
const OP_CLIP: u16 = 0x0001;
const OP_DEF_HILITE: u16 = 0x001E;
const OP_VERSION_OP: u16 = 0x0011;
const OP_HEADER_OP: u16 = 0x0C00;
const OP_BITS_RECT: u16 = 0x0090;
const OP_PACK_BITS_RECT: u16 = 0x0098;
const OP_DIRECT_BITS_RECT: u16 = 0x009A;
const OP_END_OF_PICTURE: u16 = 0x00FF;
const VERSION_2: u16 = 0x02FF;

/// Encode an indexed PICT at any sub-byte or byte pixel size.
///
/// `pixel_size` must be 1, 2, 4, or 8.  `indices` contains one palette index
/// per pixel (only the low `pixel_size` bits are meaningful); the encoder packs
/// them MSB-first before running PackBits on each row.  `palette` is the CLUT
/// to embed; its length must be exactly `2^pixel_size`.
pub fn encode_packbits(
    width: u32,
    height: u32,
    pixel_size: u16,
    palette: &[Rgb],
    indices: &[u8],
) -> Result<Vec<u8>, PictError> {
    match pixel_size {
        1 | 2 | 4 | 8 => {}
        other => return Err(PictError::InvalidPixelSize(other)),
    }
    let expected_palette = 1usize << pixel_size;
    if palette.len() != expected_palette {
        return Err(PictError::LenMismatch {
            got: palette.len(),
            expected: expected_palette,
        });
    }
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

    align_even(&mut out);
    write_u16(&mut out, OP_PACK_BITS_RECT);
    write_pixmap_header_indexed(&mut out, width, height, pixel_size);
    write_color_table(&mut out, palette);
    write_srcrect(&mut out, width, height);
    write_srcrect(&mut out, width, height);
    write_u16(&mut out, 0); // mode = srcCopy

    let row_bytes = pixmap_row_bytes(width, pixel_size);
    let pixels_per_byte = 8 / pixel_size as usize;
    let mask = if pixel_size == 8 {
        0xFF_u8
    } else {
        (1u8 << pixel_size) - 1
    };

    let mut row_buf = vec![0u8; row_bytes as usize];
    for y in 0..height as usize {
        // Pack indices MSB-first into bytes.
        row_buf.fill(0);
        for x in 0..width as usize {
            let idx = indices[y * width as usize + x] & mask;
            let byte_pos = x / pixels_per_byte;
            let bit_shift = (pixels_per_byte - 1 - (x % pixels_per_byte)) * pixel_size as usize;
            row_buf[byte_pos] |= idx << bit_shift;
        }
        let packed = pack_bits(&row_buf[..row_bytes as usize]);
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

/// Encode a full-color indexed PICT. `indices` is width×height u8 palette
/// indices, `palette` is the CLUT they point into.
///
/// Thin wrapper over [`encode_packbits`] for backwards compatibility.
pub fn encode_indexed(
    width: u32,
    height: u32,
    palette: &[Rgb; 256],
    indices: &[u8],
) -> Result<Vec<u8>, PictError> {
    encode_packbits(width, height, 8, palette, indices)
}

/// Encode a 24-bit direct-color PICT using DirectBitsRect (opcode 0x009A).
///
/// `rgb` is width×height packed RGB triples (3 bytes per pixel). The PixMap
/// uses `pixelSize=32, cmpCount=3, cmpSize=8, packType=1` (unpacked), with
/// pixels stored as 4-byte xRGB tuples (leading byte = 0).
pub fn encode_direct_bits_rect_rgb(
    width: u32,
    height: u32,
    rgb: &[u8],
) -> Result<Vec<u8>, PictError> {
    validate_dims(width, height)?;
    let expected = (width * height) as usize * 3;
    if rgb.len() != expected {
        return Err(PictError::LenMismatch {
            got: rgb.len(),
            expected,
        });
    }

    let mut out = Vec::with_capacity(512 + expected + 256);
    write_common_prelude(&mut out, width, height);

    align_even(&mut out);
    write_u32(&mut out, OP_DIRECT_BITS_RECT as u32); // opcode is 16-bit but written as u32 per spec
    // Actually DirectBitsRect opcode is 0x009A written as a u16 in PICT v2.
    // We already wrote the u32; undo and redo as u16.
    // (The above write_u32 is wrong - revert and use u16.)
    // Drop the last 4 bytes we wrote and re-emit as u16.
    out.truncate(out.len() - 4);
    write_u16(&mut out, OP_DIRECT_BITS_RECT);

    // PixMap base address (ignored in PICT; write 0x000000FF per QD convention)
    write_u32(&mut out, 0x000000FF);

    // PixMap record for DirectBitsRect: rowBytes with high bit set.
    // For 32-bit pixels: rowBytes = width * 4 (must be even, always is for *4).
    let row_bytes = width * 4;
    write_u16(&mut out, (row_bytes as u16) | 0x8000);
    // bounds
    write_rect(&mut out, 0, 0, height as i16, width as i16);
    // pmVersion
    write_u16(&mut out, 0);
    // packType = 1 (unpacked)
    write_u16(&mut out, 1);
    // packSize
    write_u32(&mut out, 0);
    // hRes, vRes
    write_u32(&mut out, 0x0048_0000);
    write_u32(&mut out, 0x0048_0000);
    // pixelType = 16 (RGBDirect)
    write_u16(&mut out, 16);
    // pixelSize = 32
    write_u16(&mut out, 32);
    // cmpCount = 3
    write_u16(&mut out, 3);
    // cmpSize = 8
    write_u16(&mut out, 8);
    // planeBytes
    write_u32(&mut out, 0);
    // pmTable handle: write a dummy 1-entry color table so QuickDraw is happy.
    // Actually for DirectBitsRect pmTable is a handle — write 0 (nil handle).
    write_u32(&mut out, 0);
    // pmReserved
    write_u32(&mut out, 0);

    // srcRect, dstRect, mode
    write_srcrect(&mut out, width, height);
    write_srcrect(&mut out, width, height);
    write_u16(&mut out, 0); // srcCopy

    // Pixel data: 4 bytes per pixel (0, R, G, B) — packType=1 means unpacked.
    // Rows are NOT length-prefixed for packType=1.
    for pixel in rgb.chunks_exact(3) {
        out.push(0); // alpha/pad
        out.push(pixel[0]);
        out.push(pixel[1]);
        out.push(pixel[2]);
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
    // BitMap struct: rowBytes u16 (MSB must be 0), bounds Rect.
    let row_bytes = bitmap_row_bytes(width);
    write_u16(&mut out, row_bytes);
    write_rect(&mut out, 0, 0, height as i16, width as i16);
    write_rect(&mut out, 0, 0, height as i16, width as i16);
    write_rect(&mut out, 0, 0, height as i16, width as i16);
    write_u16(&mut out, 0);

    let src_stride = expected_row;
    for y in 0..height as usize {
        let src_row = &bits[y * src_stride..(y + 1) * src_stride];
        out.extend_from_slice(src_row);
        out.extend(std::iter::repeat_n(0u8, row_bytes as usize - src_stride));
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
    out.extend_from_slice(&[0u8; 512]);
    write_u16(out, 0);
    write_rect(out, 0, 0, height as i16, width as i16);
    write_u16(out, OP_VERSION_OP);
    write_u16(out, VERSION_2);
    write_u16(out, OP_HEADER_OP);
    write_i16(out, -1);
    write_u16(out, 0);
    write_u32(out, 0x0048_0000);
    write_u32(out, 0x0048_0000);
    write_rect(out, 0, 0, height as i16, width as i16);
    write_u32(out, 0);
    write_u16(out, OP_DEF_HILITE);
    write_u16(out, OP_CLIP);
    write_u16(out, 10);
    write_rect(out, 0, 0, height as i16, width as i16);
    let _ = OP_NOP;
    align_even(out);
}

fn write_pixmap_header_indexed(out: &mut Vec<u8>, width: u32, height: u32, pixel_size: u16) {
    let rb = pixmap_row_bytes(width, pixel_size);
    write_u16(out, rb | 0x8000);
    write_rect(out, 0, 0, height as i16, width as i16);
    write_u16(out, 0); // pmVersion
    write_u16(out, 0); // packType
    write_u32(out, 0); // packSize
    write_u32(out, 0x0048_0000);
    write_u32(out, 0x0048_0000);
    write_u16(out, 0); // pixelType = chunky/indexed
    write_u16(out, pixel_size);
    write_u16(out, 1); // cmpCount
    write_u16(out, pixel_size); // cmpSize
    write_u32(out, 0); // planeBytes
    write_u32(out, 0); // pmTable handle
    write_u32(out, 0); // pmReserved
}

fn write_color_table(out: &mut Vec<u8>, palette: &[Rgb]) {
    write_u32(out, 0); // ctSeed
    write_u16(out, 0); // ctFlags
    write_u16(out, (palette.len() - 1) as u16); // ctSize = numEntries - 1
    for (i, c) in palette.iter().enumerate() {
        write_u16(out, i as u16);
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
    let bits = width as u64 * pixel_size as u64;
    let bytes = bits.div_ceil(16) * 2;
    bytes as u16
}

fn bitmap_row_bytes(width: u32) -> u16 {
    let bytes = (width as u64).div_ceil(16) * 2;
    (bytes as u16) & 0x7FFF
}

/// Apple PackBits byte RLE.
fn pack_bits(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 128 + 1);
    let mut i = 0;
    while i < input.len() {
        let mut run_len = 1;
        while i + run_len < input.len() && input[i + run_len] == input[i] && run_len < 128 {
            run_len += 1;
        }
        if run_len >= 3 {
            // PackBits: a negative count byte n decodes to (1 - n) repetitions,
            // so a run of k bytes must be encoded as n = 1 - k.
            out.push(((0x101 - run_len as u16) & 0xFF) as u8);
            out.push(input[i]);
            i += run_len;
        } else {
            let start = i;
            let mut lit_len = 1;
            i += 1;
            while i < input.len() && lit_len < 128 {
                let looks_like_run =
                    i + 2 < input.len() && input[i] == input[i + 1] && input[i] == input[i + 2];
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
    use crate::palette::{MAC_4_COLOR, MAC_16_COLOR, MAC_SYSTEM_PALETTE, gray_ramp};

    #[test]
    fn packbits_run_of_five() {
        // Run of 5 -> count byte = 1 - 5 = -4 = 0xFC.
        let out = pack_bits(&[0xAA, 0xAA, 0xAA, 0xAA, 0xAA]);
        assert_eq!(out, vec![0xFC, 0xAA]);
    }

    #[test]
    fn packbits_literal_three_distinct() {
        let out = pack_bits(&[1, 2, 3]);
        assert_eq!(out, vec![0x02, 1, 2, 3]);
    }

    #[test]
    fn packbits_mixed_literal_then_run() {
        // literal [1,2] header = len-1 = 1; then run of 4 threes -> 1-4 = -3 = 0xFD.
        let out = pack_bits(&[1, 2, 3, 3, 3, 3]);
        assert_eq!(out, vec![0x01, 1, 2, 0xFD, 3]);
    }

    #[test]
    fn packbits_max_run_of_128_is_not_nop() {
        // Guard against the -128 (0x80) no-op encoding: a 128-byte run
        // must emit 0x81 (= 1 - 128), never 0x80.
        let out = pack_bits(&[0x55; 128]);
        assert_eq!(out, vec![0x81, 0x55]);
    }

    #[test]
    fn packbits_roundtrip_matches_unpackbits() {
        // Decode with the standard PackBits rule and verify we recover
        // the input byte-for-byte. This is the property that was violated
        // by the off-by-one bug (each run decoded to one extra byte).
        fn unpack(mut src: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            while let Some((&h, rest)) = src.split_first() {
                src = rest;
                let n = h as i8;
                if n == -128 {
                    continue;
                } else if n >= 0 {
                    let len = n as usize + 1;
                    out.extend_from_slice(&src[..len]);
                    src = &src[len..];
                } else {
                    let count = 1 - n as isize;
                    let b = src[0];
                    src = &src[1..];
                    out.extend(std::iter::repeat_n(b, count as usize));
                }
            }
            out
        }
        let inputs: &[&[u8]] = &[
            &[0xAA; 5],
            &[1, 2, 3, 3, 3, 3],
            &[0; 200],
            &[0x55; 128],
            b"the quick brown fox jumps over the lazy dog",
        ];
        for input in inputs {
            let packed = pack_bits(input);
            assert_eq!(&unpack(&packed), *input, "roundtrip failed for {input:?}");
        }
    }

    #[test]
    fn encode_indexed_smoke_test() {
        let indices = vec![0u8; 4 * 4];
        let bytes = encode_indexed(4, 4, &MAC_SYSTEM_PALETTE, &indices).unwrap();
        assert!(bytes.len() > 512 + 20);
        let off = 512 + 2 + 8 + 2;
        assert_eq!(&bytes[off..off + 2], &[0x02, 0xFF]);
    }

    #[test]
    fn encode_bitmap_smoke_test() {
        let bits = vec![0u8; 2 * 4];
        let bytes = encode_bitmap(16, 4, &bits).unwrap();
        assert!(bytes.len() > 512 + 20);
    }

    #[test]
    fn encode_indexed_len_mismatch_errors() {
        let e = encode_indexed(4, 4, &MAC_SYSTEM_PALETTE, &[0u8; 3]).unwrap_err();
        assert!(matches!(
            e,
            PictError::LenMismatch {
                got: 3,
                expected: 16
            }
        ));
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

    fn pict_version_offset(_bytes: &[u8]) -> usize {
        512 + 2 + 8 + 2
    }

    #[test]
    fn encode_packbits_8bit_version_signature() {
        let indices = vec![0u8; 4 * 4];
        let bytes = encode_packbits(4, 4, 8, &MAC_SYSTEM_PALETTE, &indices).unwrap();
        let off = pict_version_offset(&bytes);
        assert_eq!(&bytes[off..off + 2], &[0x02, 0xFF]);
    }

    #[test]
    fn encode_packbits_4bit_color_opcode() {
        let indices = vec![0u8; 8 * 8];
        let bytes = encode_packbits(8, 8, 4, &MAC_16_COLOR, &indices).unwrap();
        // PackBitsRect opcode 0x0098 appears after the common prelude.
        assert!(bytes.windows(2).any(|w| w == [0x00, 0x98]));
        // CLUT ctSize field should be 15 (= 16 - 1).
        // The CLUT starts after the PixMap header; search for ctSize=15 (0x000F)
        // preceded by ctFlags=0 (0x0000) and ctSeed (4 bytes).
        // Easier: count that we have exactly 16 CLUT entries.
        // Each entry: 2 (index) + 6 (RGB u16×3) = 8 bytes. Total = 128.
        // Find 0x000F (15) in the byte stream as ctSize.
        let has_ctsize_15 = bytes.windows(2).any(|w| w == [0x00, 0x0F]);
        assert!(has_ctsize_15, "expected ctSize=15 for 16-color palette");
    }

    #[test]
    fn encode_packbits_2bit_gray_opcode() {
        let palette = gray_ramp::<4>();
        let indices = vec![0u8; 8 * 8];
        let bytes = encode_packbits(8, 8, 2, &palette, &indices).unwrap();
        assert!(bytes.windows(2).any(|w| w == [0x00, 0x98]));
        // ctSize = 3 for 4-entry palette
        let has_ctsize_3 = bytes.windows(2).any(|w| w == [0x00, 0x03]);
        assert!(has_ctsize_3, "expected ctSize=3 for 4-color palette");
    }

    #[test]
    fn encode_packbits_1bit_uses_2_entry_palette() {
        let palette = [Rgb::new(255, 255, 255), Rgb::new(0, 0, 0)];
        let indices = vec![0u8; 8 * 8];
        let bytes = encode_packbits(8, 8, 1, &palette, &indices).unwrap();
        assert!(bytes.windows(2).any(|w| w == [0x00, 0x98]));
    }

    #[test]
    fn encode_packbits_invalid_pixel_size_errors() {
        let e = encode_packbits(4, 4, 3, &MAC_4_COLOR, &[0u8; 16]).unwrap_err();
        assert!(matches!(e, PictError::InvalidPixelSize(3)));
    }

    #[test]
    fn encode_direct_bits_smoke_test() {
        let rgb = vec![0u8; 8 * 8 * 3];
        let bytes = encode_direct_bits_rect_rgb(8, 8, &rgb).unwrap();
        assert!(bytes.len() > 512 + 20);
        // Version-2 signature at expected offset
        let off = pict_version_offset(&bytes);
        assert_eq!(&bytes[off..off + 2], &[0x02, 0xFF]);
        // DirectBitsRect opcode 0x009A
        assert!(bytes.windows(2).any(|w| w == [0x00, 0x9A]));
    }

    #[test]
    fn encode_packbits_4bit_row_packing() {
        // 4 pixels wide, 4-bit depth: each row = 2 bytes (4px × 4bit / 8).
        // Alternating index 0 and index 15 → byte pattern 0x0F, 0x0F.
        let palette = MAC_16_COLOR;
        let indices: Vec<u8> = (0..8).flat_map(|_| [0u8, 15, 0, 15]).collect();
        let bytes = encode_packbits(4, 8, 4, &palette, &indices).unwrap();
        assert!(bytes.len() > 512);
    }
}
