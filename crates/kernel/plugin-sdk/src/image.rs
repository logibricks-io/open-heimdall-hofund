//! Bounded fitting for inline images.

use std::io::Cursor;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{DynamicImage, ImageEncoder as _, ImageFormat, ImageReader};

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
pub fn fit_base64(encoded: &str, limit: usize) -> Result<Fitted, FitError> {
    if encoded.len() > MAX_INPUT {
        return Err(FitError::InputTooLarge);
    }
    let raw = match STANDARD.decode(encoded) {
        Ok(raw) => raw,
        Err(_) => return Err(FitError::Unreadable),
    };
    let reader = match ImageReader::new(Cursor::new(raw.as_slice())).with_guessed_format() {
        Ok(reader) => reader,
        Err(_) => return Err(FitError::Unreadable),
    };
    let format = match reader.format() {
        Some(format) => format,
        None => return Err(FitError::Unreadable),
    };
    let (width, height) = match reader.into_dimensions() {
        Ok(dimensions) => dimensions,
        Err(_) => return Err(FitError::Unreadable),
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
    let mut next = f64::from(edge) * 0.7;
    while next >= f64::from(MIN_EDGE) {
        tries.push((next.round() as u32, lossy(75)));
        next *= 0.7;
    }
    for (edge, output) in tries {
        if started.elapsed() > DEADLINE {
            return Err(FitError::Deadline);
        }
        let decoded = match image::load_from_memory(&raw) {
            Ok(decoded) => decoded,
            Err(_) => return Err(FitError::ResizeFailed),
        };
        let decoded = if edge < long {
            decoded.resize(edge, edge, image::imageops::FilterType::Triangle)
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
                .ok()?
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
