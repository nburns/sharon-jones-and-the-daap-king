//! Error-diffusion dithering. Two algorithms, both scanning in serpentine
//! (boustrophedon) order to avoid the directional worm artifacts you get
//! from monotonic left-to-right diffusion.
//!
//!   * [`floyd_steinberg_indexed`] — RGB → 8-bit indexed against a fixed
//!     palette. 100% of the quantization error propagates to 4 neighbours.
//!   * [`atkinson_1bit`]          — grayscale → black/white. 75% of the
//!     error propagates to 6 neighbours (1/8 each); the missing 25% is
//!     discarded on purpose, brightening the output slightly. This is the
//!     algorithm Bill Atkinson designed for the original Mac and used in
//!     MacPaint / HyperCard; it gives 1-bit images a characteristic
//!     high-contrast "Mac look".

use crate::palette::{nearest_index, Rgb};

/// Floyd-Steinberg dither RGB → indexed. Input is width×height RGB pixels;
/// output is width×height palette indices. Serpentine scan.
pub fn floyd_steinberg_indexed(
    width: u32,
    height: u32,
    rgb: &[Rgb],
    palette: &[Rgb; 256],
) -> Vec<u8> {
    assert_eq!(rgb.len(), (width * height) as usize);
    // Work in i16 so accumulated error can go negative/large without
    // saturation until we snap back at output time.
    let mut buf: Vec<[i16; 3]> = rgb
        .iter()
        .map(|p| [p.r as i16, p.g as i16, p.b as i16])
        .collect();
    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0u8; w * h];

    for y in 0..h {
        let ltr = y % 2 == 0;
        // Iterate x in the current row's scan direction.
        let xs: Box<dyn Iterator<Item = usize>> = if ltr {
            Box::new(0..w)
        } else {
            Box::new((0..w).rev())
        };
        for x in xs {
            let idx_here = y * w + x;
            let p = buf[idx_here];
            let clamp = |v: i16| -> u8 {
                if v < 0 {
                    0
                } else if v > 255 {
                    255
                } else {
                    v as u8
                }
            };
            let snapped = Rgb::new(clamp(p[0]), clamp(p[1]), clamp(p[2]));
            let pi = nearest_index(palette, snapped);
            out[idx_here] = pi;
            let picked = palette[pi as usize];
            let err = [
                p[0] - picked.r as i16,
                p[1] - picked.g as i16,
                p[2] - picked.b as i16,
            ];
            // Distribute error. Neighbour offsets differ depending on scan
            // direction so serpentine scanning always spreads "forward"
            // (into un-processed pixels).
            //
            // Left→right (canonical FS):
            //   [+1, 0] 7/16, [-1,+1] 3/16, [0,+1] 5/16, [+1,+1] 1/16
            // Right→left (mirror):
            //   [-1, 0] 7/16, [+1,+1] 3/16, [0,+1] 5/16, [-1,+1] 1/16
            let (dx_forward, mirror) = if ltr { (1i32, 1i32) } else { (-1i32, -1i32) };
            let spread = |buf: &mut [[i16; 3]], nx: i32, ny: i32, num: i16| {
                if nx < 0 || nx >= w as i32 || ny >= h as i32 {
                    return;
                }
                let ni = (ny as usize) * w + (nx as usize);
                for c in 0..3 {
                    // Multiply-then-divide to keep sign correct.
                    let d = (err[c] * num) / 16;
                    buf[ni][c] = buf[ni][c].saturating_add(d);
                }
            };
            let x_i = x as i32;
            let y_i = y as i32;
            spread(&mut buf, x_i + dx_forward, y_i, 7);
            spread(&mut buf, x_i - mirror, y_i + 1, 3);
            spread(&mut buf, x_i, y_i + 1, 5);
            spread(&mut buf, x_i + mirror, y_i + 1, 1);
        }
    }
    out
}

/// Atkinson dither grayscale → 1-bit. Input is width×height luminance
/// values (0..=255); output is width×height booleans as a bit-packed
/// row-major `Vec<u8>` in **MSB-first order per byte**, with rows padded
/// to the next full byte (i.e. row stride = `(width + 7) / 8`).
pub fn atkinson_1bit(width: u32, height: u32, gray: &[u8]) -> Vec<u8> {
    assert_eq!(gray.len(), (width * height) as usize);
    let w = width as usize;
    let h = height as usize;
    let mut buf: Vec<i16> = gray.iter().map(|&v| v as i16).collect();

    let row_stride = w.div_ceil(8);
    let mut out = vec![0u8; row_stride * h];

    for y in 0..h {
        let ltr = y % 2 == 0;
        let xs: Box<dyn Iterator<Item = usize>> = if ltr {
            Box::new(0..w)
        } else {
            Box::new((0..w).rev())
        };
        for x in xs {
            let idx_here = y * w + x;
            let v = buf[idx_here];
            let black = v < 128;
            // Bit i in output row: MSB is x=0.
            if !black {
                let byte_i = y * row_stride + x / 8;
                out[byte_i] |= 0x80 >> (x & 7);
            }
            let output_v: i16 = if black { 0 } else { 255 };
            // Error divided by 8; only 6 neighbours receive it (total 6/8).
            let e = (v - output_v) / 8;
            let mirror: i32 = if ltr { 1 } else { -1 };
            let scatter = |buf: &mut [i16], nx: i32, ny: i32| {
                if nx < 0 || nx >= w as i32 || ny >= h as i32 {
                    return;
                }
                let ni = (ny as usize) * w + (nx as usize);
                buf[ni] = buf[ni].saturating_add(e);
            };
            let x_i = x as i32;
            let y_i = y as i32;
            // Atkinson neighbours (relative to canonical LTR):
            //   [+1, 0] [+2, 0]
            //   [-1,+1] [ 0,+1] [+1,+1]
            //   [ 0,+2]
            scatter(&mut buf, x_i + mirror, y_i);
            scatter(&mut buf, x_i + 2 * mirror, y_i);
            scatter(&mut buf, x_i - mirror, y_i + 1);
            scatter(&mut buf, x_i, y_i + 1);
            scatter(&mut buf, x_i + mirror, y_i + 1);
            scatter(&mut buf, x_i, y_i + 2);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Rgb, MAC_SYSTEM_PALETTE};

    #[test]
    fn fs_all_black_stays_black() {
        let w = 8;
        let h = 8;
        let src = vec![Rgb::new(0, 0, 0); (w * h) as usize];
        let out = floyd_steinberg_indexed(w, h, &src, &MAC_SYSTEM_PALETTE);
        for &i in &out {
            assert_eq!(MAC_SYSTEM_PALETTE[i as usize], Rgb::new(0, 0, 0));
        }
    }

    #[test]
    fn fs_all_white_stays_white() {
        let w = 4;
        let h = 4;
        let src = vec![Rgb::new(255, 255, 255); (w * h) as usize];
        let out = floyd_steinberg_indexed(w, h, &src, &MAC_SYSTEM_PALETTE);
        for &i in &out {
            assert_eq!(MAC_SYSTEM_PALETTE[i as usize], Rgb::new(255, 255, 255));
        }
    }

    #[test]
    fn atkinson_pure_black_input_all_zero_bits() {
        let out = atkinson_1bit(16, 4, &vec![0u8; 64]);
        assert!(out.iter().all(|&b| b == 0));
        assert_eq!(out.len(), 2 * 4); // (16+7)/8 = 2 bytes/row × 4 rows
    }

    #[test]
    fn atkinson_pure_white_input_all_ones_within_width() {
        // 8-wide row: full byte of 0xFF, no padding bits.
        let out = atkinson_1bit(8, 2, &vec![255u8; 16]);
        assert_eq!(out, vec![0xFF, 0xFF]);
    }

    #[test]
    fn atkinson_msb_first_layout() {
        // Single pixel white in the top-left corner, everything else black.
        let mut src = vec![0u8; 8 * 1];
        src[0] = 255;
        let out = atkinson_1bit(8, 1, &src);
        // MSB-first means x=0 goes in the top bit → 0b1000_0000.
        assert_eq!(out[0] & 0x80, 0x80);
        // (other bits may pick up diffused error rounding; don't assert them)
    }

    #[test]
    fn atkinson_row_stride_pads_to_byte() {
        // 5px wide → stride 1 byte; 12px wide → stride 2 bytes; 16px → 2.
        assert_eq!(atkinson_1bit(5, 1, &vec![0u8; 5]).len(), 1);
        assert_eq!(atkinson_1bit(12, 1, &vec![0u8; 12]).len(), 2);
        assert_eq!(atkinson_1bit(16, 1, &vec![0u8; 16]).len(), 2);
    }
}
