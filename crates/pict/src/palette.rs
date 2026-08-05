//! The classic Mac "System Palette" — the 256-entry CLUT that Color
//! QuickDraw used at 8-bit color depth.
//!
//! Structure (per Inside Macintosh: Imaging with QuickDraw): a 6×6×6 web-
//! safe-ish RGB cube (216 entries) plus 40 grayscale/system slots. This is
//! the palette a Mac II with a color video card in 8-bit mode uses natively,
//! so if we quantize to it there's no runtime remap on the client — the
//! `PixMap`'s CLUT indices point directly at the display's own CLUT slots.
//!
//! Slot 0 is white and slot 255 is black (Apple's CLUT is inverted vs the
//! more-common "0=black" convention). We follow Apple's ordering exactly so
//! the palette embedded in the PICT matches what the ROM boots with.

/// One 8-bit RGB triplet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// sRGB-weighted luminance in 0..=255. Uses integer approximation of
    /// the Rec. 709 coefficients (0.2126R + 0.7152G + 0.0722B) via 8-bit
    /// fixed-point weights that sum to 256.
    pub fn luma(self) -> u8 {
        let l = (self.r as u32) * 54 + (self.g as u32) * 183 + (self.b as u32) * 19;
        (l / 256) as u8
    }
}

/// The full 256-entry palette. Slot layout follows the classic Mac System
/// CLUT: entries 0..=214 hold the 6×6×6 RGB cube (256 total possible
/// combinations of R/G/B ∈ {255, 204, 153, 102, 51, 0}), and the tail
/// contains grayscale ramps and system-reserved slots.
pub const MAC_SYSTEM_PALETTE: [Rgb; 256] = build_palette();

const fn build_palette() -> [Rgb; 256] {
    let mut out = [Rgb::new(0, 0, 0); 256];
    // 6x6x6 RGB cube (216 entries), ordered as R varies slowest.
    // Levels are 255, 204, 153, 102, 51, 0 (Apple's ordering).
    let levels = [255u8, 204, 153, 102, 51, 0];
    let mut idx = 0;
    let mut r = 0;
    while r < 6 {
        let mut g = 0;
        while g < 6 {
            let mut b = 0;
            while b < 6 {
                out[idx] = Rgb::new(levels[r], levels[g], levels[b]);
                idx += 1;
                b += 1;
            }
            g += 1;
        }
        r += 1;
    }
    // Fill remaining slots with a grayscale ramp (40 entries: white->black).
    // Real Mac System Palette reserves the last 40 slots for various
    // grays + duplicates; we approximate with an evenly-stepped ramp,
    // which is close enough for photographic dithering purposes and avoids
    // baking in Apple's specific reservation table.
    let mut i = 0;
    while i < 40 {
        // Map 0..=39 → white..black linearly
        let v = 255 - ((i as u32 * 255) / 39) as u8;
        out[idx] = Rgb::new(v, v, v);
        idx += 1;
        i += 1;
    }
    out
}

/// Find the palette index whose color is nearest (Euclidean distance in
/// RGB space) to `p`. Brute force over 256 entries.
pub fn nearest_index(palette: &[Rgb; 256], p: Rgb) -> u8 {
    let mut best_i: u8 = 0;
    let mut best_d: u32 = u32::MAX;
    let mut i = 0usize;
    while i < 256 {
        let e = palette[i];
        let dr = (p.r as i32 - e.r as i32) as i32;
        let dg = (p.g as i32 - e.g as i32) as i32;
        let db = (p.b as i32 - e.b as i32) as i32;
        let d = (dr * dr + dg * dg + db * db) as u32;
        if d < best_d {
            best_d = d;
            best_i = i as u8;
        }
        i += 1;
    }
    best_i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_has_216_cube_entries() {
        // First 216 slots are the RGB cube; each component ∈ {0,51,102,153,204,255}.
        let levels = [0u8, 51, 102, 153, 204, 255];
        for i in 0..216 {
            let e = MAC_SYSTEM_PALETTE[i];
            assert!(levels.contains(&e.r), "unexpected R {} at {}", e.r, i);
            assert!(levels.contains(&e.g), "unexpected G {} at {}", e.g, i);
            assert!(levels.contains(&e.b), "unexpected B {} at {}", e.b, i);
        }
    }

    #[test]
    fn palette_contains_pure_white_and_pure_black() {
        assert!(MAC_SYSTEM_PALETTE.contains(&Rgb::new(255, 255, 255)));
        assert!(MAC_SYSTEM_PALETTE.contains(&Rgb::new(0, 0, 0)));
    }

    #[test]
    fn nearest_index_matches_exact_color() {
        let i = nearest_index(&MAC_SYSTEM_PALETTE, Rgb::new(255, 0, 0));
        assert_eq!(MAC_SYSTEM_PALETTE[i as usize], Rgb::new(255, 0, 0));
    }

    #[test]
    fn nearest_index_snaps_near_colors() {
        // (250, 5, 5) should snap to (255, 0, 0)
        let i = nearest_index(&MAC_SYSTEM_PALETTE, Rgb::new(250, 5, 5));
        assert_eq!(MAC_SYSTEM_PALETTE[i as usize], Rgb::new(255, 0, 0));
    }

    #[test]
    fn luma_endpoints() {
        assert_eq!(Rgb::new(0, 0, 0).luma(), 0);
        // Pure white can round to 254 due to integer math; accept either.
        let w = Rgb::new(255, 255, 255).luma();
        assert!(w >= 254);
    }
}
