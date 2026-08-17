//! Encoder for the classic Mac PICT image format (Version 2).
//!
//! Supported depth/mode combinations (the "matrix"):
//!
//! | depth | mode  | Encoder              | Opcode       |
//! |-------|-------|----------------------|--------------|
//! | 1     | -     | BitsRect + Atkinson  | 0x0090       |
//! | 2     | gray  | PackBitsRect 2-bit   | 0x0098       |
//! | 2     | color | PackBitsRect 2-bit   | 0x0098       |
//! | 4     | gray  | PackBitsRect 4-bit   | 0x0098       |
//! | 4     | color | PackBitsRect 4-bit   | 0x0098       |
//! | 8     | gray  | PackBitsRect 8-bit   | 0x0098       |
//! | 8     | color | PackBitsRect 8-bit   | 0x0098       |
//! | 24    | -     | DirectBitsRect       | 0x009A       |
//!
//! Companion modules:
//!   * [`palette`] — System Palette, 16-color, 4-color, gray ramp helpers.
//!   * [`dither`]  — Floyd-Steinberg, Atkinson, Bayer ordered dithers.
//!
//! No runtime deps. All output is big-endian.

pub mod dither;
pub mod encoder;
pub mod palette;

pub use encoder::{encode_bitmap, encode_direct_bits_rect_rgb, encode_indexed, encode_packbits};
pub use palette::{MAC_4_COLOR, MAC_16_COLOR, MAC_SYSTEM_PALETTE, Rgb, gray_ramp};

/// Error returned by encode functions.
#[derive(Debug, thiserror::Error)]
pub enum PictError {
    #[error("image dimensions must be > 0")]
    ZeroDim,
    #[error("width {0} exceeds PICT max (32767)")]
    WidthTooLarge(u32),
    #[error("height {0} exceeds PICT max (32767)")]
    HeightTooLarge(u32),
    #[error("indices length ({got}) doesn't match width×height ({expected})")]
    LenMismatch { got: usize, expected: usize },
    #[error("pixel size {0} is not valid (must be 1, 2, 4, or 8)")]
    InvalidPixelSize(u16),
}
