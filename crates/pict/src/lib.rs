//! Encoder for the classic Mac PICT image format (Version 2).
//!
//! Two entry points:
//!   * [`encode_indexed`] — 8-bit indexed color with a caller-supplied
//!     256-entry RGB palette. Emits `PackBitsRect` opcode.
//!   * [`encode_bitmap`]  — 1-bit black/white. Emits `BitsRect` opcode.
//!
//! Companion modules:
//!   * [`palette`] — the classic Mac System Palette as a `[Rgb; 256]` const.
//!   * [`dither`]  — Floyd-Steinberg (indexed) and Atkinson (1-bit) dithers,
//!                   both serpentine.
//!
//! No runtime deps. All output is big-endian (PICT convention).

pub mod dither;
pub mod encoder;
pub mod palette;

pub use encoder::{encode_bitmap, encode_indexed};
pub use palette::{Rgb, MAC_SYSTEM_PALETTE};

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
}
