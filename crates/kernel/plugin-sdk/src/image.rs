//! Bounded fitting for inline images.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{
    ColorType, DynamicImage, ImageEncoder as _, ImageFormat, ImageReader, Rgba, RgbaImage,
};

use crate::digest::sha256;

const LONG_EDGE: u32 = 2_000;
const MIN_EDGE: u32 = 256;
const MAX_PIXELS: u64 = 50_000_000;
const MAX_INPUT: usize = 64 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);

/// A successfully fitted image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fitted {
    /// Wire format name.
    pub format: &'static str,
    /// Encoded image bytes.
    pub base64: String,
}

/// Why an image could not be fitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FitError {
    /// The encoded input exceeded the inspection cap.
    InputTooLarge,
    /// The declared dimensions exceeded the pixel ceiling.
    TooManyPixels,
    /// The bytes were not a readable supported image.
    Unreadable,
    /// Decoding or encoding failed after the header was read.
    ResizeFailed,
    /// Every attempt still exceeded the requested Base64 limit.
    StillTooLarge,
    /// The fitting deadline elapsed.
    Deadline,
}

/// Re-encodes an image and reduces its long edge until its Base64 fits `limit`.
///
/// PNG uses maximum compression. JPEG and WebP start at quality 85, followed by
/// quality 80 and then quality 75 at successively smaller dimensions. PNG and
/// other input formats use lossy WebP after the first attempt; JPEG stays JPEG.
///
/// # Errors
///
/// Returns [`FitError`] when the input exceeds a safety limit, cannot be
/// decoded or encoded, exceeds the deadline, or remains over `limit`.
pub fn fit_base64(encoded: &str, limit: usize) -> Result<Fitted, FitError> {
    if encoded.len() > MAX_INPUT {
        return Err(FitError::InputTooLarge);
    }
    let Ok(raw) = STANDARD.decode(encoded) else {
        return Err(FitError::Unreadable);
    };
    let Ok(reader) = ImageReader::new(Cursor::new(raw.as_slice())).with_guessed_format() else {
        return Err(FitError::Unreadable);
    };
    let Some(format) = reader.format() else {
        return Err(FitError::Unreadable);
    };
    let Ok((width, height)) = reader.into_dimensions() else {
        return Err(FitError::Unreadable);
    };
    if width == 0 || height == 0 {
        return Err(FitError::Unreadable);
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(FitError::TooManyPixels);
    }

    let started = Instant::now();
    let long = width.max(height);
    let edge = long.min(LONG_EDGE);
    let jpeg = format == ImageFormat::Jpeg;
    let own = if jpeg {
        Output::Jpeg(85)
    } else if format == ImageFormat::WebP {
        Output::WebP(85)
    } else {
        Output::Png
    };
    let lossy = |quality| {
        if jpeg {
            Output::Jpeg(quality)
        } else {
            Output::WebP(quality)
        }
    };
    let mut tries = vec![(edge, own), (edge, lossy(80))];
    let mut numerator = u64::from(edge) * 7;
    let mut denominator = 10_u64;
    while numerator >= u64::from(MIN_EDGE) * denominator {
        let Ok(rounded) = u32::try_from((numerator + denominator / 2) / denominator) else {
            return Err(FitError::ResizeFailed);
        };
        tries.push((rounded, lossy(75)));
        numerator *= 7;
        denominator *= 10;
    }
    for (edge, output) in tries {
        if started.elapsed() > DEADLINE {
            return Err(FitError::Deadline);
        }
        let Ok(decoded) = image::load_from_memory(&raw) else {
            return Err(FitError::ResizeFailed);
        };
        let decoded = if edge < long {
            decoded.resize(edge, edge, image::imageops::FilterType::Lanczos3)
        } else {
            decoded
        };
        let Some(bytes) = encode(&decoded, output) else {
            return Err(FitError::ResizeFailed);
        };
        let base64 = STANDARD.encode(bytes);
        if base64.len() <= limit {
            return Ok(Fitted {
                format: output.name(),
                base64,
            });
        }
    }
    Err(FitError::StillTooLarge)
}

#[derive(Clone, Copy)]
enum Output {
    Png,
    Jpeg(u8),
    WebP(u8),
}

impl Output {
    fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg(_) => "jpeg",
            Self::WebP(_) => "webp",
        }
    }
}

fn encode(image: &DynamicImage, output: Output) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    match output {
        Output::Png => {
            PngEncoder::new_with_quality(&mut bytes, CompressionType::Best, FilterType::Adaptive)
                .write_image(
                    image.as_bytes(),
                    image.width(),
                    image.height(),
                    image.color().into(),
                )
                .ok()?;
        }
        Output::Jpeg(quality) => JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode_image(image)
            .ok()?,
        Output::WebP(quality) => {
            let rgba = image.to_rgba8();
            let encoded = webp::Encoder::from_rgba(rgba.as_raw(), image.width(), image.height())
                .encode(f32::from(quality));
            bytes.extend_from_slice(&encoded);
        }
    }
    Some(bytes)
}

/// Long edge and base64 budget the reference applies before Claude Code sends an image.
const CLAUDE_SIDE: u32 = 2_000;
/// Five mebibytes of base64 text.
const CLAUDE_BASE64_LIMIT: usize = 5 << 20;
/// Inspection cap for inline base64 before decode (reference `codexImageMaxBytes`).
const CLAUDE_INSPECTION_CAP: usize = 320 << 20;

/// Media type and base64 data for a Claude Code inline image block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeImage {
    /// Wire media type (for example `image/jpeg`).
    pub media_type: String,
    /// Standard base64 payload without a `data:` prefix.
    pub data: String,
}

/// Fits an inline image the way Claude Code does before `-p` stream-json upload.
///
/// Images within 2000 px per side and five mebibytes of base64 go unchanged,
/// including unreadable or oversize inputs that cannot be decoded safely.
///
/// # Panics
///
/// Panics if the in-process fit cache mutex is poisoned.
#[must_use]
pub fn fit_claude(media_type: &str, encoded: &str) -> ClaudeImage {
    let pass = || ClaudeImage {
        media_type: media_type.to_owned(),
        data: encoded.to_owned(),
    };
    if encoded.is_empty() || encoded.len() > CLAUDE_INSPECTION_CAP {
        return pass();
    }
    let Ok(raw_len) = dimensions_from_base64(encoded) else {
        return pass();
    };
    let (width, height) = raw_len;
    if width <= CLAUDE_SIDE && height <= CLAUDE_SIDE && encoded.len() <= CLAUDE_BASE64_LIMIT {
        return pass();
    }
    let key = claude_fit_key(media_type, encoded);
    if let Some(hit) = claude_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return hit.clone();
    }
    let fitted = claude_image_refit(encoded).unwrap_or_else(pass);
    claude_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remember(key, fitted.clone());
    fitted
}

fn dimensions_from_base64(encoded: &str) -> Result<(u32, u32), ()> {
    let Ok(raw) = STANDARD.decode(encoded) else {
        return Err(());
    };
    let Ok(reader) = ImageReader::new(Cursor::new(raw)).with_guessed_format() else {
        return Err(());
    };
    reader.into_dimensions().map_err(|_| ())
}

fn claude_fit_key(media_type: &str, encoded: &str) -> [u8; 32] {
    let mut seed = Vec::with_capacity(16 + media_type.len() + encoded.len() + 1);
    seed.extend_from_slice(&CLAUDE_SIDE.to_le_bytes());
    seed.extend_from_slice(&(-1_i64).to_le_bytes());
    seed.extend_from_slice(media_type.as_bytes());
    seed.push(b';');
    seed.extend_from_slice(encoded.as_bytes());
    sha256(&seed)
}

struct ClaudeFitCache {
    map: HashMap<[u8; 32], ClaudeImage>,
}

impl ClaudeFitCache {
    fn get(&self, key: &[u8; 32]) -> Option<&ClaudeImage> {
        self.map.get(key)
    }

    fn remember(&mut self, key: [u8; 32], value: ClaudeImage) {
        if self.map.len() >= 8_192 {
            return;
        }
        self.map.entry(key).or_insert(value);
    }
}

fn claude_cache() -> &'static Mutex<ClaudeFitCache> {
    static CACHE: LazyLock<Mutex<ClaudeFitCache>> = LazyLock::new(|| {
        Mutex::new(ClaudeFitCache {
            map: HashMap::new(),
        })
    });
    &CACHE
}

fn claude_image_refit(encoded: &str) -> Option<ClaudeImage> {
    let Ok(raw) = STANDARD.decode(encoded) else {
        return None;
    };
    let format = image::guess_format(&raw).ok()?;
    let Ok(decoded) = image::load_from_memory(&raw) else {
        return None;
    };
    let width = decoded.width();
    let height = decoded.height();
    let holds = image_holds(width, height, format, decoded.color())?;
    if raw.len().saturating_add(holds) > CLAUDE_INSPECTION_CAP {
        return None;
    }
    let (target_w, target_h) = claude_target_size(width, height);
    let resized = if target_w == width && target_h == height {
        decoded
    } else {
        decoded.resize_exact(target_w, target_h, image::imageops::FilterType::Triangle)
    };
    let jpeg_source = format == ImageFormat::Jpeg;
    let fits = |bytes: &[u8]| encoded_base64_len(bytes.len()) <= CLAUDE_BASE64_LIMIT;
    if jpeg_source {
        if let Some(bytes) = encode_jpeg(&resized, 85)
            && fits(&bytes)
        {
            return Some(ClaudeImage {
                media_type: "image/jpeg".into(),
                data: STANDARD.encode(bytes),
            });
        }
    } else if let Some(bytes) = encode_png_simple(&resized)
        && fits(&bytes)
    {
        return Some(ClaudeImage {
            media_type: "image/png".into(),
            data: STANDARD.encode(bytes),
        });
    }
    let flat = flatten_on_white(&resized);
    for quality in [75_u8, 60, 40] {
        if let Some(bytes) = encode_jpeg(&flat, quality)
            && fits(&bytes)
        {
            return Some(ClaudeImage {
                media_type: "image/jpeg".into(),
                data: STANDARD.encode(bytes),
            });
        }
    }
    None
}

fn claude_target_size(width: u32, height: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= CLAUDE_SIDE {
        return (width, height);
    }
    let scale = f64::from(CLAUDE_SIDE) / f64::from(long);
    (
        claude_scaled_edge(width, scale),
        claude_scaled_edge(height, scale),
    )
}

fn claude_scaled_edge(edge: u32, scale: f64) -> u32 {
    let scaled = (f64::from(edge) * scale).round();
    if !scaled.is_finite() || scaled <= 0.0 {
        return 1;
    }
    if scaled >= f64::from(u32::MAX) {
        return u32::MAX;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let out = scaled as u32;
    out.max(1)
}

fn encoded_base64_len(byte_len: usize) -> usize {
    byte_len.div_ceil(3) * 4
}

fn image_holds(width: u32, height: u32, format: ImageFormat, color: ColorType) -> Option<usize> {
    if width == 0 || height == 0 {
        return None;
    }
    let bytes_per_pixel = match color {
        ColorType::L8 => 1,
        ColorType::L16 if format == ImageFormat::Png => 2,
        ColorType::Rgb16 | ColorType::Rgba16 | ColorType::La16 if format == ImageFormat::Png => 8,
        _ => 4,
    };
    width
        .checked_mul(height)?
        .checked_mul(u32::try_from(bytes_per_pixel).ok()?)
        .map(usize::try_from)
        .and_then(Result::ok)
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, quality)
        .encode_image(image)
        .ok()?;
    Some(bytes)
}

fn encode_png_simple(image: &DynamicImage) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(
            image.as_bytes(),
            image.width(),
            image.height(),
            image.color().into(),
        )
        .ok()?;
    Some(bytes)
}

fn flatten_on_white(image: &DynamicImage) -> DynamicImage {
    let src = image.to_rgba8();
    let mut flat = RgbaImage::from_pixel(image.width(), image.height(), Rgba([255, 255, 255, 255]));
    image::imageops::overlay(&mut flat, &src, 0, 0);
    DynamicImage::ImageRgba8(flat)
}

#[cfg(test)]
mod claude_tests {
    #![allow(clippy::cast_possible_truncation)]

    use super::{CLAUDE_BASE64_LIMIT, CLAUDE_SIDE, ClaudeImage, FitError, fit_base64, fit_claude};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageEncoder as _, Rgb, RgbImage};

    fn tall_jpeg(w: u32, h: u32) -> String {
        let mut image = RgbImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                image.put_pixel(
                    x,
                    y,
                    Rgb([(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8]),
                );
            }
        }
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 85)
            .write_image(image.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .expect("jpeg");
        STANDARD.encode(bytes)
    }

    fn decode_config(b64: &str) -> (u32, u32, String) {
        let raw = STANDARD.decode(b64).expect("base64");
        let format = image::guess_format(&raw).expect("format");
        let reader = image::ImageReader::new(std::io::Cursor::new(raw))
            .with_guessed_format()
            .expect("reader");
        let (w, h) = reader.into_dimensions().expect("dims");
        (w, h, format_to_mime(format))
    }

    fn format_to_mime(format: image::ImageFormat) -> String {
        match format {
            image::ImageFormat::Jpeg => "jpeg".into(),
            image::ImageFormat::Png => "png".into(),
            other => format!("{other:?}").to_lowercase(),
        }
    }

    #[test]
    fn r698_a_tall_jpeg_scales_to_98_by_2000() {
        let raw = tall_jpeg(400, 8192);
        let fitted = fit_claude("image/jpeg", &raw);
        let (w, h, format) = decode_config(&fitted.data);
        assert_eq!(w, 98, "width");
        assert_eq!(h, CLAUDE_SIDE, "height");
        assert_eq!(format, "jpeg");
        assert_eq!(fitted.media_type, "image/jpeg");
    }

    #[test]
    fn r698_a_wide_png_becomes_2000_by_667_and_stays_png() {
        let image = image::ImageBuffer::from_fn(3000, 1000, |_, _| image::Luma([0_u8]));
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(image.as_raw(), 3000, 1000, image::ExtendedColorType::L8)
            .expect("png");
        let wide = STANDARD.encode(bytes);
        let fitted = fit_claude("image/png", &wide);
        let (w, h, format) = decode_config(&fitted.data);
        assert_eq!((w, h), (2000, 667));
        assert_eq!(format, "png");
        assert_eq!(fitted.media_type, "image/png");
    }

    #[test]
    fn r698_an_image_within_limits_is_unchanged() {
        let small = tall_jpeg(400, 2000);
        let fitted = fit_claude("image/jpeg", &small);
        assert_eq!(
            fitted,
            ClaudeImage {
                media_type: "image/jpeg".into(),
                data: small,
            }
        );
    }

    #[test]
    fn r698_unreadable_base64_is_unchanged() {
        let junk = "bm90IGFuIGltYWdl";
        let fitted = fit_claude("image/png", junk);
        assert_eq!(
            fitted,
            ClaudeImage {
                media_type: "image/png".into(),
                data: junk.into(),
            }
        );
    }

    #[test]
    fn r698_oversize_base64_may_fall_back_to_jpeg_quality() {
        let image = RgbImage::from_fn(CLAUDE_SIDE, CLAUDE_SIDE, |x, y| {
            Rgb([
                ((x * 3) ^ (y * 7)) as u8,
                ((x + y) % 256) as u8,
                ((x ^ y).wrapping_mul(17)) as u8,
            ])
        });
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(
                image.as_raw(),
                CLAUDE_SIDE,
                CLAUDE_SIDE,
                image::ExtendedColorType::Rgb8,
            )
            .expect("png");
        let encoded = STANDARD.encode(bytes);
        assert!(
            encoded.len() > CLAUDE_BASE64_LIMIT,
            "fixture should exceed the Claude base64 budget"
        );
        let fitted = fit_claude("image/png", &encoded);
        let (_, _, format) = decode_config(&fitted.data);
        assert_eq!(fitted.media_type, "image/jpeg");
        assert_eq!(format, "jpeg");
        assert!(fitted.data.len() <= CLAUDE_BASE64_LIMIT);
    }

    #[test]
    fn r698_kiro_fit_base64_stays_on_its_own_path() {
        let small = tall_jpeg(400, 2000);
        assert!(fit_base64(&small, CLAUDE_BASE64_LIMIT).is_ok());
        let garbage = "not-image";
        assert_eq!(
            fit_base64(garbage, CLAUDE_BASE64_LIMIT),
            Err(FitError::Unreadable)
        );
    }
}
