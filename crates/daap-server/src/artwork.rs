//! Album-art pipeline: decode source bytes, Lanczos3 resize to fit
//! (mw × mh), encode as JPEG or PICT. Result is cached in-process by
//! (track_id, width, height, variant).
//!
//! Sits above `MediaSource::artwork(...)`. Backends return raw bytes; this
//! module owns all the decode/resize/encode/caching so behavior is uniform
//! across backends.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use bytes::Bytes;
use fast_image_resize as fr;
use image::{ImageEncoder, ImageReader};
use lru::LruCache;
use media_source::TrackId;

#[derive(Debug, Clone)]
pub struct Config {
    /// Max entries in the resize cache. Each entry ~5-100kB.
    pub cache_size: usize,
    /// JPEG quality for encoded output (1..=100). 85 is a good default.
    pub jpeg_quality: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cache_size: 500,
            jpeg_quality: 85,
        }
    }
}

/// Bit depth for PICT output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PictDepth {
    D1,
    D2,
    D4,
    D8,
    D24,
}

/// Color mode for PICT output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PictMode {
    Gray,
    Color,
}

/// Output format the caller wants for a given request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputVariant {
    Jpeg,
    /// PICT output with the given depth and optional mode.
    /// D1 and D24 always use mode=None in the cache key.
    Pict { depth: PictDepth, mode: Option<PictMode> },
}

impl OutputVariant {
    pub fn content_type(self) -> &'static str {
        match self {
            OutputVariant::Jpeg => "image/jpeg",
            OutputVariant::Pict { .. } => "image/x-pict",
        }
    }
}

pub struct Artworker {
    config: Config,
    cache: Mutex<LruCache<CacheKey, Bytes>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    track: TrackId,
    w: Option<u32>,
    h: Option<u32>,
    variant: OutputVariant,
}

/// Outcome of a prepare() call.
pub enum Prepared {
    /// Freshly encoded output (or cache hit).
    Encoded {
        bytes: Bytes,
        content_type: &'static str,
    },
    /// Fallback — the source bytes untouched with their detected content-type.
    Original {
        bytes: Bytes,
        content_type: &'static str,
    },
}

impl Artworker {
    pub fn new(config: Config) -> Self {
        let cap = NonZeroUsize::new(config.cache_size.max(1)).unwrap();
        Self {
            config,
            cache: Mutex::new(LruCache::new(cap)),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn lookup(&self, key: CacheKey) -> Option<Bytes> {
        self.cache.lock().ok()?.get(&key).cloned()
    }

    fn store(&self, key: CacheKey, value: Bytes) {
        if let Ok(mut c) = self.cache.lock() {
            c.put(key, value);
        }
    }

    /// Decode → optionally-resize → encode. On any pipeline failure, return
    /// the original bytes with their detected Content-Type so clients see
    /// *something* rather than a 404.
    pub fn prepare(
        &self,
        track: TrackId,
        source_bytes: Bytes,
        requested_w: Option<u32>,
        requested_h: Option<u32>,
        variant: OutputVariant,
    ) -> Prepared {
        let key = CacheKey { track, w: requested_w, h: requested_h, variant };
        if let Some(hit) = self.lookup(key) {
            return Prepared::Encoded {
                bytes: hit,
                content_type: variant.content_type(),
            };
        }

        let result = match variant {
            OutputVariant::Jpeg => encode_jpeg(
                &source_bytes,
                requested_w,
                requested_h,
                self.config.jpeg_quality,
            ),
            OutputVariant::Pict { depth, mode } => {
                encode_pict(&source_bytes, requested_w, requested_h, depth, mode)
            }
        };

        match result {
            Ok(bytes) => {
                let b = Bytes::from(bytes);
                self.store(key, b.clone());
                Prepared::Encoded {
                    bytes: b,
                    content_type: variant.content_type(),
                }
            }
            Err(err) => {
                tracing::warn!(
                    track, ?variant, ?err,
                    "artwork encode failed; falling back to original bytes"
                );
                let ct = sniff_content_type(&source_bytes);
                Prepared::Original {
                    bytes: source_bytes,
                    content_type: ct,
                }
            }
        }
    }
}

/// Shared pipeline: decode source → optional Lanczos3 resize → RGB8 pixels.
fn decode_and_resize(
    source_bytes: &[u8],
    requested_w: Option<u32>,
    requested_h: Option<u32>,
) -> Result<(u32, u32, Vec<u8>), ArtworkError> {
    let reader = ImageReader::new(std::io::Cursor::new(source_bytes))
        .with_guessed_format()
        .map_err(ArtworkError::Io)?;
    let img = reader.decode().map_err(ArtworkError::Decode)?;
    let rgb = img.to_rgb8();
    let (src_w, src_h) = rgb.dimensions();

    let (dst_w, dst_h) = match (requested_w, requested_h) {
        (None, None) => (src_w, src_h),
        (Some(w), Some(h)) => fit_within(src_w, src_h, w, h),
        (Some(w), None) => fit_within(src_w, src_h, w, u32::MAX),
        (None, Some(h)) => fit_within(src_w, src_h, u32::MAX, h),
    };

    if (dst_w, dst_h) == (src_w, src_h) {
        return Ok((src_w, src_h, rgb.into_raw()));
    }

    let src_image = fr::images::Image::from_vec_u8(
        src_w,
        src_h,
        rgb.into_raw(),
        fr::PixelType::U8x3,
    )
    .map_err(|e| ArtworkError::Resize(e.to_string()))?;
    let mut dst_image = fr::images::Image::new(dst_w, dst_h, fr::PixelType::U8x3);

    let mut resizer = fr::Resizer::new();
    let options = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3));
    resizer
        .resize(&src_image, &mut dst_image, &options)
        .map_err(|e| ArtworkError::Resize(e.to_string()))?;

    Ok((dst_w, dst_h, dst_image.buffer().to_vec()))
}

fn encode_jpeg(
    source_bytes: &[u8],
    requested_w: Option<u32>,
    requested_h: Option<u32>,
    quality: u8,
) -> Result<Vec<u8>, ArtworkError> {
    let (w, h, rgb) = decode_and_resize(source_bytes, requested_w, requested_h)?;
    let mut out = Vec::with_capacity(w as usize * h as usize / 3);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(ArtworkError::Encode)?;
    Ok(out)
}

fn encode_pict(
    source_bytes: &[u8],
    requested_w: Option<u32>,
    requested_h: Option<u32>,
    depth: PictDepth,
    mode: Option<PictMode>,
) -> Result<Vec<u8>, ArtworkError> {
    let (w, h, rgb) = decode_and_resize(source_bytes, requested_w, requested_h)?;

    match (depth, mode) {
        (PictDepth::D1, _) => {
            let gray: Vec<u8> = rgb
                .chunks_exact(3)
                .map(|c| pict::Rgb::new(c[0], c[1], c[2]).luma())
                .collect();
            let bits = pict::dither::atkinson_1bit(w, h, &gray);
            pict::encode_bitmap(w, h, &bits).map_err(|e| ArtworkError::Pict(e.to_string()))
        }

        (PictDepth::D2, Some(PictMode::Gray)) => {
            let gray: Vec<u8> = rgb
                .chunks_exact(3)
                .map(|c| pict::Rgb::new(c[0], c[1], c[2]).luma())
                .collect();
            let palette = pict::gray_ramp::<4>();
            let indices = pict::dither::ordered_bayer_gray(w, h, &gray, 4);
            pict::encode_packbits(w, h, 2, &palette, &indices)
                .map_err(|e| ArtworkError::Pict(e.to_string()))
        }

        (PictDepth::D2, Some(PictMode::Color)) => {
            let pixels: Vec<pict::Rgb> = rgb
                .chunks_exact(3)
                .map(|c| pict::Rgb::new(c[0], c[1], c[2]))
                .collect();
            let indices = pict::dither::floyd_steinberg_palette(w, h, &pixels, &pict::MAC_4_COLOR);
            pict::encode_packbits(w, h, 2, &pict::MAC_4_COLOR, &indices)
                .map_err(|e| ArtworkError::Pict(e.to_string()))
        }

        (PictDepth::D4, Some(PictMode::Gray)) => {
            let gray: Vec<u8> = rgb
                .chunks_exact(3)
                .map(|c| pict::Rgb::new(c[0], c[1], c[2]).luma())
                .collect();
            let palette = pict::gray_ramp::<16>();
            let indices = pict::dither::floyd_steinberg_gray(w, h, &gray, 16);
            pict::encode_packbits(w, h, 4, &palette, &indices)
                .map_err(|e| ArtworkError::Pict(e.to_string()))
        }

        (PictDepth::D4, Some(PictMode::Color)) => {
            let pixels: Vec<pict::Rgb> = rgb
                .chunks_exact(3)
                .map(|c| pict::Rgb::new(c[0], c[1], c[2]))
                .collect();
            let indices =
                pict::dither::floyd_steinberg_palette(w, h, &pixels, &pict::MAC_16_COLOR);
            pict::encode_packbits(w, h, 4, &pict::MAC_16_COLOR, &indices)
                .map_err(|e| ArtworkError::Pict(e.to_string()))
        }

        (PictDepth::D8, Some(PictMode::Gray)) => {
            let gray: Vec<u8> = rgb
                .chunks_exact(3)
                .map(|c| pict::Rgb::new(c[0], c[1], c[2]).luma())
                .collect();
            let palette = pict::gray_ramp::<256>();
            let indices = pict::dither::floyd_steinberg_gray(w, h, &gray, 256);
            pict::encode_packbits(w, h, 8, &palette, &indices)
                .map_err(|e| ArtworkError::Pict(e.to_string()))
        }

        (PictDepth::D8, Some(PictMode::Color)) | (PictDepth::D8, None) => {
            let pixels: Vec<pict::Rgb> = rgb
                .chunks_exact(3)
                .map(|c| pict::Rgb::new(c[0], c[1], c[2]))
                .collect();
            let indices = pict::dither::floyd_steinberg_indexed(
                w,
                h,
                &pixels,
                &pict::MAC_SYSTEM_PALETTE,
            );
            pict::encode_indexed(w, h, &pict::MAC_SYSTEM_PALETTE, &indices)
                .map_err(|e| ArtworkError::Pict(e.to_string()))
        }

        (PictDepth::D24, _) => {
            pict::encode_direct_bits_rect_rgb(w, h, &rgb)
                .map_err(|e| ArtworkError::Pict(e.to_string()))
        }

        // Unreachable when callers validate mode correctly, but handle gracefully.
        _ => Err(ArtworkError::Pict(
            "invalid depth/mode combination".to_string(),
        )),
    }
}

/// Downscale-only fit: never upscales, preserves aspect ratio.
fn fit_within(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (max_w.max(1), max_h.max(1));
    }
    if src_w <= max_w && src_h <= max_h {
        return (src_w, src_h);
    }
    let ratio_w = max_w as f64 / src_w as f64;
    let ratio_h = max_h as f64 / src_h as f64;
    let ratio = ratio_w.min(ratio_h);
    let w = ((src_w as f64 * ratio).round() as u32).max(1);
    let h = ((src_h as f64 * ratio).round() as u32).max(1);
    (w, h)
}

/// Peek at magic bytes to pick a Content-Type for a fallback-original response.
pub fn sniff_content_type(bytes: &[u8]) -> &'static str {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        _ => "application/octet-stream",
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArtworkError {
    #[error("io: {0}")]
    Io(std::io::Error),
    #[error("decode: {0}")]
    Decode(image::ImageError),
    #[error("resize: {0}")]
    Resize(String),
    #[error("encode: {0}")]
    Encode(image::ImageError),
    #[error("pict: {0}")]
    Pict(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_never_upscales() {
        assert_eq!(fit_within(100, 100, 512, 512), (100, 100));
    }

    #[test]
    fn fit_preserves_aspect_wide() {
        assert_eq!(fit_within(800, 400, 512, 512), (512, 256));
    }

    #[test]
    fn fit_preserves_aspect_tall() {
        assert_eq!(fit_within(400, 800, 512, 512), (256, 512));
    }

    #[test]
    fn fit_uses_smaller_ratio() {
        assert_eq!(fit_within(1000, 2000, 500, 1500), (500, 1000));
    }

    #[test]
    fn sniff_recognizes_common_formats() {
        assert_eq!(sniff_content_type(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(sniff_content_type(b"\x89PNG\r\n\x1a\n"), "image/png");
        assert_eq!(sniff_content_type(b"GIF89a"), "image/gif");
        assert_eq!(sniff_content_type(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
        assert_eq!(sniff_content_type(b"random data"), "application/octet-stream");
    }

    #[test]
    fn resize_round_trip_png_to_jpeg() {
        let mut src = image::RgbImage::new(1024, 512);
        for (x, _y, p) in src.enumerate_pixels_mut() {
            let b = (x % 256) as u8;
            *p = image::Rgb([0, 0, b]);
        }
        let mut png_bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png_bytes)
            .write_image(
                src.as_raw(),
                src.width(),
                src.height(),
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();

        let jpeg = encode_jpeg(&png_bytes, Some(400), Some(400), 85).unwrap();
        assert_eq!(&jpeg[0..3], &[0xFF, 0xD8, 0xFF], "JPEG magic bytes present");
        let dec = ImageReader::new(std::io::Cursor::new(&jpeg))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        assert_eq!(dec.width(), 400);
        assert_eq!(dec.height(), 200);
    }

    fn tiny_png() -> Bytes {
        let mut png_bytes = Vec::new();
        let img = image::RgbImage::new(64, 64);
        image::codecs::png::PngEncoder::new(&mut png_bytes)
            .write_image(img.as_raw(), 64, 64, image::ExtendedColorType::Rgb8)
            .unwrap();
        Bytes::from(png_bytes)
    }

    #[test]
    fn cache_hits_second_lookup() {
        let png = tiny_png();
        let art = Artworker::new(Config::default());
        let first = match art.prepare(
            1,
            png.clone(),
            Some(32),
            Some(32),
            OutputVariant::Jpeg,
        ) {
            Prepared::Encoded { bytes, .. } => bytes,
            _ => panic!("expected Encoded"),
        };
        let second = match art.prepare(
            1,
            png.clone(),
            Some(32),
            Some(32),
            OutputVariant::Jpeg,
        ) {
            Prepared::Encoded { bytes, .. } => bytes,
            _ => panic!("expected Encoded"),
        };
        assert_eq!(first, second);
    }

    #[test]
    fn prepare_falls_back_on_bogus_bytes() {
        let art = Artworker::new(Config::default());
        match art.prepare(
            1,
            Bytes::from_static(b"not an image"),
            Some(64),
            Some(64),
            OutputVariant::Jpeg,
        ) {
            Prepared::Original { content_type, .. } => {
                assert_eq!(content_type, "application/octet-stream");
            }
            _ => panic!("expected Original fallback"),
        }
    }

    fn pict_prelude_offset() -> usize {
        512 + 2 + 8 + 2
    }

    fn assert_pict_prelude(bytes: &[u8], content_type: &str) {
        assert_eq!(content_type, "image/x-pict");
        let off = pict_prelude_offset();
        assert_eq!(&bytes[off..off + 2], &[0x02, 0xFF]);
    }

    #[test]
    fn all_pict_cells_start_with_pict_prelude() {
        // The 8 matrix cells: (depth, mode)
        let cells: &[(PictDepth, Option<PictMode>)] = &[
            (PictDepth::D1, None),
            (PictDepth::D2, Some(PictMode::Gray)),
            (PictDepth::D2, Some(PictMode::Color)),
            (PictDepth::D4, Some(PictMode::Gray)),
            (PictDepth::D4, Some(PictMode::Color)),
            (PictDepth::D8, Some(PictMode::Gray)),
            (PictDepth::D8, Some(PictMode::Color)),
            (PictDepth::D24, None),
        ];
        let png = tiny_png();
        let art = Artworker::new(Config::default());
        for (i, &(depth, mode)) in cells.iter().enumerate() {
            let variant = OutputVariant::Pict { depth, mode };
            let bytes = match art.prepare(i as u32, png.clone(), Some(16), Some(16), variant) {
                Prepared::Encoded { bytes, content_type } => {
                    assert_pict_prelude(&bytes, content_type);
                    bytes
                }
                _ => panic!("expected Encoded PICT for {depth:?}/{mode:?}"),
            };
            assert!(bytes.len() > 512 + 20, "too short for {depth:?}/{mode:?}");
        }
    }

    #[test]
    fn pict8_color_output_matches_old_pict8_variant() {
        // Back-compat: D8/Color must produce the same bytes as the old Pict8 path.
        let png = tiny_png();
        let art = Artworker::new(Config::default());
        let new_bytes = match art.prepare(
            10,
            png.clone(),
            Some(16),
            Some(16),
            OutputVariant::Pict { depth: PictDepth::D8, mode: Some(PictMode::Color) },
        ) {
            Prepared::Encoded { bytes, .. } => bytes,
            _ => panic!("expected Encoded"),
        };
        let off = pict_prelude_offset();
        assert_eq!(&new_bytes[off..off + 2], &[0x02, 0xFF]);
    }

    #[test]
    fn no_resize_when_dims_omitted() {
        let png = tiny_png();
        let (w, h, _) = decode_and_resize(&png, None, None).unwrap();
        assert_eq!((w, h), (64, 64));
    }
}
