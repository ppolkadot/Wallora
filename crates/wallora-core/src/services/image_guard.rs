use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::Path;
use std::sync::Arc;

use image::ImageReader;
use image::error::{ImageError, LimitErrorKind};
use image::{ImageFormat, Limits};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::provider::ImageClass;

/// Maximum width or height (in pixels) permitted for thumbnails.
pub const THUMBNAIL_MAX_SIDE: u32 = 1024;
/// Maximum total pixels permitted for thumbnails (`1024 * 1024`).
pub const THUMBNAIL_MAX_PIXELS: u64 = 1024 * 1024;
/// Maximum width or height (in pixels) permitted for previews and previewed originals.
pub const PREVIEW_MAX_SIDE: u32 = 16_384;
/// Maximum total pixels permitted for previews (`150 MP`).
pub const PREVIEW_MAX_PIXELS: u64 = 150_000_000;
/// Default decoded RGBA8 byte budget for textures in flight (640 MiB, accommodating one 150 MP preview).
pub const DEFAULT_DECODED_BYTES_BUDGET: usize = 640 * 1024 * 1024;

const HEADER_MAX_ALLOC: u64 = 16 * 1024 * 1024;
const THUMBNAIL_SECOND_LAYER_MAX_ALLOC: u64 = 32 * 1024 * 1024;
const PREVIEW_SECOND_LAYER_MAX_ALLOC: u64 = 1_280 * 1024 * 1024;
const DECODE_SCRATCH_HEADROOM: u64 = 8 * 1024 * 1024;

/// Per-class encoded size, dimension, and pixel limits enforced before decode allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBudget {
    pub max_encoded_bytes: usize,
    pub max_side: u32,
    pub max_pixels: u64,
}

impl ImageBudget {
    pub const fn for_class(class: ImageClass) -> Self {
        match class {
            ImageClass::Thumbnail => Self {
                max_encoded_bytes: class.byte_limit(),
                max_side: THUMBNAIL_MAX_SIDE,
                max_pixels: THUMBNAIL_MAX_PIXELS,
            },
            ImageClass::Preview => Self {
                max_encoded_bytes: class.byte_limit(),
                max_side: PREVIEW_MAX_SIDE,
                max_pixels: PREVIEW_MAX_PIXELS,
            },
        }
    }
}

/// Returns the default global decode concurrency: `max(1, cores / 2)`.
pub fn default_decode_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(1)
        .max(1)
}

/// Configuration for [`ImageGuard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageGuardConfig {
    pub decode_concurrency: usize,
    pub decoded_bytes_budget: usize,
}

impl Default for ImageGuardConfig {
    fn default() -> Self {
        Self {
            decode_concurrency: default_decode_concurrency(),
            decoded_bytes_budget: DEFAULT_DECODED_BYTES_BUDGET,
        }
    }
}

/// Validated image header metadata obtained without allocating pixel buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageHeader {
    pub width: u32,
    pub height: u32,
    pub decoded_rgba_bytes: u64,
    pub format: ImageFormat,
}

/// Errors produced by [`ImageGuard`] validation or decoding.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ImageGuardError {
    #[error("image payload is empty")]
    EmptyPayload,
    #[error("encoded image ({actual} bytes) exceeds the {limit} byte limit")]
    EncodedTooLarge { actual: u64, limit: usize },
    #[error("could not identify a supported image format")]
    UnsupportedFormat,
    #[error("image has invalid zero dimensions ({width}x{height})")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("image dimensions ({width}x{height}) exceed the {max_side}px per-side limit")]
    DimensionsExceeded {
        width: u32,
        height: u32,
        max_side: u32,
    },
    #[error("image pixel count ({pixels}) exceeds the {max_pixels} pixel limit")]
    MegapixelsExceeded { pixels: u64, max_pixels: u64 },
    #[error(
        "decoded RGBA size ({required} bytes) exceeds the {budget} byte in-flight texture budget"
    )]
    DecodedBudgetExceeded { required: u64, budget: usize },
    #[error("image decoder hit resource limit: {0}")]
    DecoderLimit(String),
    #[error(
        "image dimensions changed during decode (header {expected_width}x{expected_height}, decoded {actual_width}x{actual_height})"
    )]
    DimensionMismatch {
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    },
    #[error("image decode failed: {0}")]
    DecodeFailed(String),
    #[error("image I/O failed: {0}")]
    Io(String),
    #[error("image decode task failed")]
    TaskCancelled,
}

/// Decoded RGBA8 texture holding its slice of the in-flight decoded-bytes budget until dropped.
#[derive(Debug)]
pub struct DecodedImage {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    _budget_permit: OwnedSemaphorePermit,
}

impl DecodedImage {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

#[derive(Debug)]
struct ImageGuardInner {
    decode_sem: Arc<Semaphore>,
    decode_concurrency: usize,
    decoded_bytes_sem: Arc<Semaphore>,
    decoded_bytes_budget: usize,
}

/// Validates image headers and enforces per-class decode budgets, global decode concurrency,
/// and an in-flight decoded texture byte budget.
#[derive(Debug, Clone)]
pub struct ImageGuard {
    inner: Arc<ImageGuardInner>,
}

impl Default for ImageGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageGuard {
    pub fn new() -> Self {
        Self::with_config(ImageGuardConfig::default())
    }

    pub fn with_config(config: ImageGuardConfig) -> Self {
        let decode_concurrency = config.decode_concurrency.max(1);
        let capped_budget = u32::try_from(config.decoded_bytes_budget).unwrap_or(u32::MAX);
        Self {
            inner: Arc::new(ImageGuardInner {
                decode_sem: Arc::new(Semaphore::new(decode_concurrency)),
                decode_concurrency,
                decoded_bytes_sem: Arc::new(Semaphore::new(capped_budget as usize)),
                decoded_bytes_budget: capped_budget as usize,
            }),
        }
    }

    pub fn decode_concurrency(&self) -> usize {
        self.inner.decode_concurrency
    }

    pub fn available_decode_permits(&self) -> usize {
        self.inner.decode_sem.available_permits()
    }

    pub fn decoded_bytes_budget(&self) -> usize {
        self.inner.decoded_bytes_budget
    }

    pub fn available_decoded_bytes(&self) -> usize {
        self.inner.decoded_bytes_sem.available_permits()
    }

    /// Performs encoded size and header-only dimension checks on in-memory image bytes
    /// without allocating pixel buffers.
    pub fn inspect_bytes(bytes: &[u8], class: ImageClass) -> Result<ImageHeader, ImageGuardError> {
        let budget = ImageBudget::for_class(class);
        check_encoded_len(bytes.len() as u64, budget.max_encoded_bytes)?;
        inspect_reader_header(Cursor::new(bytes), budget)
    }

    /// Validates header budgets, acquires in-flight texture memory and decode concurrency permits,
    /// and decodes `bytes` to RGBA8 off the async executor with `image::Limits` as a second layer.
    pub async fn decode_bytes(
        &self,
        bytes: Vec<u8>,
        class: ImageClass,
    ) -> Result<DecodedImage, ImageGuardError> {
        let budget = ImageBudget::for_class(class);
        check_encoded_len(bytes.len() as u64, budget.max_encoded_bytes)?;
        let header = inspect_reader_header(Cursor::new(bytes.as_slice()), budget)?;

        let limits = second_layer_limits(class, budget, header.decoded_rgba_bytes);
        self.decode_guarded(header, move || {
            decode_from_reader(Cursor::new(bytes), header, limits)
        })
        .await
    }

    /// Validates an on-disk image file (for example an original wallpaper previewed under
    /// [`ImageClass::Preview`]) and decodes it to RGBA8 under the same budgets and limits.
    pub async fn decode_file(
        &self,
        path: impl AsRef<Path>,
        class: ImageClass,
    ) -> Result<DecodedImage, ImageGuardError> {
        let path = path.as_ref().to_path_buf();
        let budget = ImageBudget::for_class(class);

        let inspect_path = path.clone();
        let header = tokio::task::spawn_blocking(move || {
            let file = File::open(&inspect_path).map_err(|e| ImageGuardError::Io(e.to_string()))?;
            let metadata = file
                .metadata()
                .map_err(|e| ImageGuardError::Io(e.to_string()))?;
            check_encoded_len(metadata.len(), budget.max_encoded_bytes)?;
            inspect_reader_header(BufReader::new(file), budget)
        })
        .await
        .map_err(|_| ImageGuardError::TaskCancelled)??;

        let limits = second_layer_limits(class, budget, header.decoded_rgba_bytes);
        self.decode_guarded(header, move || {
            let file = File::open(&path).map_err(|e| ImageGuardError::Io(e.to_string()))?;
            let metadata = file
                .metadata()
                .map_err(|e| ImageGuardError::Io(e.to_string()))?;
            check_encoded_len(metadata.len(), budget.max_encoded_bytes)?;
            decode_from_reader(BufReader::new(file), header, limits)
        })
        .await
    }

    async fn decode_guarded(
        &self,
        header: ImageHeader,
        decode: impl FnOnce() -> Result<Vec<u8>, ImageGuardError> + Send + 'static,
    ) -> Result<DecodedImage, ImageGuardError> {
        let budget_permit = self
            .acquire_decoded_budget(header.decoded_rgba_bytes)
            .await?;
        let decode_permit = self
            .inner
            .decode_sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ImageGuardError::TaskCancelled)?;

        tokio::task::spawn_blocking(move || {
            let _decode_guard = decode_permit;
            let rgba = decode()?;
            Ok(DecodedImage {
                width: header.width,
                height: header.height,
                rgba,
                _budget_permit: budget_permit,
            })
        })
        .await
        .map_err(|_| ImageGuardError::TaskCancelled)?
    }

    async fn acquire_decoded_budget(
        &self,
        required_bytes: u64,
    ) -> Result<OwnedSemaphorePermit, ImageGuardError> {
        let budget = self.inner.decoded_bytes_budget;
        if required_bytes > budget as u64 {
            return Err(ImageGuardError::DecodedBudgetExceeded {
                required: required_bytes,
                budget,
            });
        }
        let permits =
            u32::try_from(required_bytes).map_err(|_| ImageGuardError::DecodedBudgetExceeded {
                required: required_bytes,
                budget,
            })?;
        self.inner
            .decoded_bytes_sem
            .clone()
            .acquire_many_owned(permits)
            .await
            .map_err(|_| ImageGuardError::TaskCancelled)
    }
}

fn check_encoded_len(len: u64, limit: usize) -> Result<(), ImageGuardError> {
    if len == 0 {
        return Err(ImageGuardError::EmptyPayload);
    }
    if len > limit as u64 {
        return Err(ImageGuardError::EncodedTooLarge { actual: len, limit });
    }
    Ok(())
}

fn inspect_reader_header<R: Read + Seek>(
    reader: R,
    budget: ImageBudget,
) -> Result<ImageHeader, ImageGuardError> {
    let buffered = BufReader::new(reader);
    let mut reader = ImageReader::new(buffered)
        .with_guessed_format()
        .map_err(|e| ImageGuardError::Io(e.to_string()))?;
    let format = reader.format().ok_or(ImageGuardError::UnsupportedFormat)?;

    // Bound header and metadata parsing before touching pixel dimensions.
    let mut header_limits = Limits::default();
    header_limits.max_image_width = Some(budget.max_side);
    header_limits.max_image_height = Some(budget.max_side);
    header_limits.max_alloc = Some(HEADER_MAX_ALLOC);
    reader.limits(header_limits);

    let (width, height) = reader
        .into_dimensions()
        .map_err(|err| map_image_error(err, budget))?;
    validate_dimensions(width, height, budget, format)
}

fn validate_dimensions(
    width: u32,
    height: u32,
    budget: ImageBudget,
    format: ImageFormat,
) -> Result<ImageHeader, ImageGuardError> {
    if width == 0 || height == 0 {
        return Err(ImageGuardError::InvalidDimensions { width, height });
    }
    if width > budget.max_side || height > budget.max_side {
        return Err(ImageGuardError::DimensionsExceeded {
            width,
            height,
            max_side: budget.max_side,
        });
    }
    let pixels = u64::from(width).checked_mul(u64::from(height)).ok_or(
        ImageGuardError::MegapixelsExceeded {
            pixels: u64::MAX,
            max_pixels: budget.max_pixels,
        },
    )?;
    if pixels > budget.max_pixels {
        return Err(ImageGuardError::MegapixelsExceeded {
            pixels,
            max_pixels: budget.max_pixels,
        });
    }
    let decoded_rgba_bytes = pixels
        .checked_mul(4)
        .ok_or_else(|| ImageGuardError::DecoderLimit("decoded size overflow".to_string()))?;
    if usize::try_from(decoded_rgba_bytes).is_err() {
        return Err(ImageGuardError::DecoderLimit(
            "decoded size exceeds platform address space".to_string(),
        ));
    }
    Ok(ImageHeader {
        width,
        height,
        decoded_rgba_bytes,
        format,
    })
}

fn second_layer_limits(class: ImageClass, budget: ImageBudget, decoded_rgba_bytes: u64) -> Limits {
    let class_cap = match class {
        ImageClass::Thumbnail => THUMBNAIL_SECOND_LAYER_MAX_ALLOC,
        ImageClass::Preview => PREVIEW_SECOND_LAYER_MAX_ALLOC,
    };
    let max_alloc = decoded_rgba_bytes
        .saturating_mul(2)
        .saturating_add(DECODE_SCRATCH_HEADROOM)
        .min(class_cap);

    let mut limits = Limits::default();
    limits.max_image_width = Some(budget.max_side);
    limits.max_image_height = Some(budget.max_side);
    limits.max_alloc = Some(max_alloc);
    limits
}

fn decode_from_reader<R: Read + Seek>(
    reader: R,
    expected: ImageHeader,
    limits: Limits,
) -> Result<Vec<u8>, ImageGuardError> {
    let mut img_reader = ImageReader::with_format(BufReader::new(reader), expected.format);
    img_reader.limits(limits);

    let dynamic = img_reader.decode().map_err(map_decode_error)?;
    let rgba = dynamic.into_rgba8();
    let (actual_width, actual_height) = rgba.dimensions();
    if actual_width != expected.width || actual_height != expected.height {
        return Err(ImageGuardError::DimensionMismatch {
            expected_width: expected.width,
            expected_height: expected.height,
            actual_width,
            actual_height,
        });
    }
    let raw = rgba.into_raw();
    if raw.len() as u64 != expected.decoded_rgba_bytes {
        return Err(ImageGuardError::DecoderLimit(
            "decoded buffer size mismatch".to_string(),
        ));
    }
    Ok(raw)
}

fn map_image_error(err: ImageError, budget: ImageBudget) -> ImageGuardError {
    match err {
        ImageError::Limits(limit) => match limit.kind() {
            LimitErrorKind::DimensionError => ImageGuardError::DecoderLimit(format!(
                "dimensions exceed {}px limit",
                budget.max_side
            )),
            LimitErrorKind::InsufficientMemory => {
                ImageGuardError::DecoderLimit("header allocation limit exceeded".to_string())
            }
            _ => ImageGuardError::DecoderLimit("resource limit exceeded".to_string()),
        },
        ImageError::Unsupported(_) => ImageGuardError::UnsupportedFormat,
        ImageError::IoError(io) => ImageGuardError::Io(io.to_string()),
        other => ImageGuardError::DecodeFailed(other.to_string()),
    }
}

fn map_decode_error(err: ImageError) -> ImageGuardError {
    match err {
        ImageError::Limits(limit) => match limit.kind() {
            LimitErrorKind::DimensionError => {
                ImageGuardError::DecoderLimit("decoder dimension limit exceeded".to_string())
            }
            LimitErrorKind::InsufficientMemory => {
                ImageGuardError::DecoderLimit("decoder allocation limit exceeded".to_string())
            }
            _ => ImageGuardError::DecoderLimit("decoder resource limit exceeded".to_string()),
        },
        ImageError::Unsupported(_) => ImageGuardError::UnsupportedFormat,
        ImageError::IoError(io) => ImageGuardError::Io(io.to_string()),
        other => ImageGuardError::DecodeFailed(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_decode_keeps_budget_until_blocking_work_returns() {
        let guard = ImageGuard::with_config(ImageGuardConfig {
            decode_concurrency: 1,
            decoded_bytes_budget: 4,
        });
        let header = ImageHeader {
            width: 1,
            height: 1,
            decoded_rgba_bytes: 4,
            format: ImageFormat::Png,
        };
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let decode = tokio::spawn({
            let guard = guard.clone();
            async move {
                guard
                    .decode_guarded(header, move || {
                        entered_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(vec![0; 4])
                    })
                    .await
            }
        });

        tokio::time::timeout(Duration::from_secs(10), entered_rx)
            .await
            .unwrap()
            .unwrap();
        decode.abort();
        assert!(matches!(decode.await, Err(err) if err.is_cancelled()));
        let available_while_blocked = guard.available_decoded_bytes();
        release_tx.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(10), guard.acquire_decoded_budget(4))
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        assert_eq!(available_while_blocked, 0);
        assert_eq!(guard.available_decoded_bytes(), 4);
    }
}
