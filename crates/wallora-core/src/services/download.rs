//! Streaming byte-exact original download and no-clobber atomic file publication (Task 6).

use std::fs::OpenOptions;
use std::future::Future;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::mpsc;
use url::Url;

use crate::domain::Wallpaper;
use crate::provider::{ImageClass, OriginalImageStream, ProviderError, WallhavenClient};
use crate::services::cache::CancelToken;
use crate::services::image_guard::{ImageGuard, ImageGuardError};
use crate::util::{numbered_collision_name, sanitize_filename, split_sanitized_stem_ext};

const MAX_COLLISION_ATTEMPTS: u32 = 10_000;

/// Failure returned by [`FilePublisher::publish_no_clobber`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PublishError {
    #[error("destination file already exists")]
    AlreadyExists,
    #[error("file publication failed: {0}")]
    Io(String),
}

/// Atomically publishes a finished partial file to `dst` without ever overwriting an existing
/// destination.
pub trait FilePublisher: Send + Sync + 'static {
    fn publish_no_clobber(&self, src: &Path, dst: &Path) -> Result<(), PublishError>;
}

/// Portable no-clobber publisher using `std::fs::hard_link` plus source unlink.
///
/// Windows production builds use `wallora_windows::WindowsFilePublisher` (`MoveFileExW` without
/// `MOVEFILE_REPLACE_EXISTING`).
#[derive(Debug, Default, Clone, Copy)]
pub struct HardLinkFilePublisher;

impl FilePublisher for HardLinkFilePublisher {
    fn publish_no_clobber(&self, src: &Path, dst: &Path) -> Result<(), PublishError> {
        match std::fs::hard_link(src, dst) {
            Ok(()) => {
                let _ = std::fs::remove_file(src);
                Ok(())
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => Err(PublishError::AlreadyExists),
            Err(err) => Err(PublishError::Io(err.to_string())),
        }
    }
}

/// Chunked writer handle for an active `.part` file.
pub trait PartWriter: Send + 'static {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn finish(self: Box<Self>) -> std::io::Result<()>;
}

/// Filesystem seam for creating and cleaning up `.part` files (injectable for disk-full tests).
pub trait DownloadFs: Send + Sync + 'static {
    fn create_part(&self, path: &Path) -> std::io::Result<Box<dyn PartWriter>>;
    fn remove_file(&self, path: &Path) -> std::io::Result<()>;
}

struct RealPartWriter(std::fs::File);

impl PartWriter for RealPartWriter {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        Write::write_all(&mut self.0, bytes)
    }

    fn finish(mut self: Box<Self>) -> std::io::Result<()> {
        Write::flush(&mut self.0)?;
        self.0.sync_all()
    }
}

/// Production [`DownloadFs`] backed by `std::fs` with `create_new(true)` staging files.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealDownloadFs;

impl DownloadFs for RealDownloadFs {
    fn create_part(&self, path: &Path) -> std::io::Result<Box<dyn PartWriter>> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        Ok(Box::new(RealPartWriter(file)))
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        std::fs::remove_file(path)
    }
}

/// Boxed future returned by [`OriginalChunkStream::next_chunk`].
pub type OriginalChunkFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, ProviderError>> + Send + 'a>>;

/// Streaming response body for a full-resolution original download.
pub trait OriginalChunkStream: Send + 'static {
    fn content_length(&self) -> Option<u64>;
    fn next_chunk<'a>(&'a mut self) -> OriginalChunkFuture<'a>;
}

/// Transport seam for streaming full-resolution original images without thumbnail/preview caps.
pub trait OriginalFetcher: Send + Sync + 'static {
    type Stream: OriginalChunkStream;

    fn open_stream(
        &self,
        url: Url,
    ) -> impl Future<Output = Result<Self::Stream, ProviderError>> + Send + 'static;
}

impl OriginalChunkStream for OriginalImageStream {
    fn content_length(&self) -> Option<u64> {
        Self::content_length(self)
    }

    fn next_chunk<'a>(&'a mut self) -> OriginalChunkFuture<'a> {
        Box::pin(Self::next_chunk(self))
    }
}

impl OriginalFetcher for WallhavenClient {
    type Stream = OriginalImageStream;

    fn open_stream(
        &self,
        url: Url,
    ) -> impl Future<Output = Result<Self::Stream, ProviderError>> + Send + 'static {
        let client = self.clone();
        async move { client.open_original_stream(&url).await }
    }
}

/// Parameters for a streaming byte-exact original download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRequest {
    pub url: Url,
    pub destination: PathBuf,
    pub expected_file_size: Option<u64>,
    pub expected_dimensions: Option<(u32, u32)>,
}

impl DownloadRequest {
    /// Builds a [`DownloadRequest`] from API wallpaper metadata and a user-chosen destination path.
    pub fn for_wallpaper(
        wallpaper: &Wallpaper,
        destination: PathBuf,
    ) -> Result<Self, DownloadError> {
        let url = Url::parse(&wallpaper.path).map_err(|_| DownloadError::Provider {
            source: ProviderError::OriginRejected,
            part_path: None,
        })?;
        Ok(Self {
            url,
            destination,
            expected_file_size: (wallpaper.file_size > 0).then_some(wallpaper.file_size),
            expected_dimensions: (wallpaper.dimension_x > 0 && wallpaper.dimension_y > 0)
                .then_some((wallpaper.dimension_x, wallpaper.dimension_y)),
        })
    }
}

/// Incremental progress update emitted while streaming an original download to `.part`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    pub bytes_written: u64,
    pub total_bytes: Option<u64>,
}

/// Successful no-clobber download publication details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOutcome {
    pub path: PathBuf,
    pub bytes_written: u64,
    pub dimensions: (u32, u32),
    pub collision_renamed: bool,
}

/// Errors produced during streaming original download, verification, or no-clobber publication.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DownloadError {
    #[error("invalid download destination: {0}")]
    InvalidDestination(String),
    #[error("download was cancelled")]
    Cancelled { part_path: Option<PathBuf> },
    #[error("{source}")]
    Provider {
        source: ProviderError,
        part_path: Option<PathBuf>,
    },
    #[error("disk is full while writing partial download: {detail}")]
    DiskFull {
        detail: String,
        part_path: Option<PathBuf>,
    },
    #[error("filesystem error during download: {detail}")]
    Io {
        detail: String,
        part_path: Option<PathBuf>,
    },
    #[error("downloaded size ({actual} bytes) did not match expected size ({expected} bytes)")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error(
        "decoded dimensions ({}x{}) did not match API metadata ({}x{})",
        actual.0,
        actual.1,
        expected.0,
        expected.1
    )]
    DimensionMismatch {
        expected: (u32, u32),
        actual: (u32, u32),
    },
    #[error("downloaded file failed image verification: {0}")]
    InvalidImage(#[from] ImageGuardError),
    #[error("could not publish downloaded file: {detail}")]
    Publish {
        detail: String,
        part_path: Option<PathBuf>,
    },
    #[error("too many colliding files already exist in the target folder")]
    CollisionLimitExceeded { part_path: Option<PathBuf> },
}

impl DownloadError {
    pub fn part_path(&self) -> Option<&Path> {
        match self {
            Self::Cancelled { part_path }
            | Self::Provider { part_path, .. }
            | Self::DiskFull { part_path, .. }
            | Self::Io { part_path, .. }
            | Self::Publish { part_path, .. }
            | Self::CollisionLimitExceeded { part_path } => part_path.as_deref(),
            Self::InvalidDestination(_)
            | Self::SizeMismatch { .. }
            | Self::DimensionMismatch { .. }
            | Self::InvalidImage(_) => None,
        }
    }

    pub fn user_message(&self) -> String {
        match self {
            Self::InvalidDestination(msg) => format!("Invalid save location: {msg}"),
            Self::Cancelled { .. } => "Download cancelled.".to_string(),
            Self::Provider { source, .. } => source.user_message(),
            Self::DiskFull { .. } => {
                "Not enough disk space to finish saving the wallpaper.".to_string()
            }
            Self::Io { detail, .. } => format!("Could not write wallpaper file: {detail}"),
            Self::SizeMismatch { expected, actual } => format!(
                "Downloaded file size ({actual} bytes) did not match expected {expected} bytes."
            ),
            Self::DimensionMismatch { expected, actual } => format!(
                "Downloaded image dimensions ({}x{}) did not match expected {}x{}.",
                actual.0, actual.1, expected.0, expected.1
            ),
            Self::InvalidImage(err) => format!("Downloaded file failed image validation: {err}"),
            Self::Publish { detail, .. } => format!("Could not finalize saved file: {detail}"),
            Self::CollisionLimitExceeded { .. } => {
                "Too many similarly named files already exist in that folder.".to_string()
            }
        }
    }
}

fn is_disk_full_error(err: &std::io::Error) -> bool {
    err.kind() == ErrorKind::StorageFull
        || matches!(err.raw_os_error(), Some(112) | Some(39) | Some(28))
}

fn map_write_io_error(err: std::io::Error, part_path: Option<PathBuf>) -> DownloadError {
    let detail = err.to_string();
    if is_disk_full_error(&err) {
        DownloadError::DiskFull { detail, part_path }
    } else {
        DownloadError::Io { detail, part_path }
    }
}

async fn remove_part_best_effort(fs: &Arc<dyn DownloadFs>, part_path: &Path) {
    let fs = Arc::clone(fs);
    let path = part_path.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || fs.remove_file(&path)).await;
}

/// Streams full-resolution originals directly to a same-directory `.part` file, verifies
/// byte-exact length and [`ImageGuard`] preview decode + dimensions, and atomically publishes
/// without ever clobbering an existing file.
pub struct OriginalDownloader<F: OriginalFetcher> {
    fetcher: Arc<F>,
    image_guard: ImageGuard,
    publisher: Arc<dyn FilePublisher>,
    fs: Arc<dyn DownloadFs>,
}

impl<F: OriginalFetcher> Clone for OriginalDownloader<F> {
    fn clone(&self) -> Self {
        Self {
            fetcher: Arc::clone(&self.fetcher),
            image_guard: self.image_guard.clone(),
            publisher: Arc::clone(&self.publisher),
            fs: Arc::clone(&self.fs),
        }
    }
}

impl<F: OriginalFetcher> OriginalDownloader<F> {
    pub fn new(
        fetcher: Arc<F>,
        image_guard: ImageGuard,
        publisher: Arc<dyn FilePublisher>,
    ) -> Self {
        Self::with_fs(fetcher, image_guard, publisher, Arc::new(RealDownloadFs))
    }

    pub fn with_fs(
        fetcher: Arc<F>,
        image_guard: ImageGuard,
        publisher: Arc<dyn FilePublisher>,
        fs: Arc<dyn DownloadFs>,
    ) -> Self {
        Self {
            fetcher,
            image_guard,
            publisher,
            fs,
        }
    }

    pub async fn download(
        &self,
        request: DownloadRequest,
        cancel: CancelToken,
    ) -> Result<DownloadOutcome, DownloadError> {
        self.download_with_progress(request, cancel, None).await
    }

    pub async fn download_with_progress(
        &self,
        request: DownloadRequest,
        cancel: CancelToken,
        progress: Option<mpsc::UnboundedSender<DownloadProgress>>,
    ) -> Result<DownloadOutcome, DownloadError> {
        let Some(parent) = request.destination.parent() else {
            return Err(DownloadError::InvalidDestination(
                "destination must include a parent directory".to_string(),
            ));
        };
        if parent.as_os_str().is_empty() {
            return Err(DownloadError::InvalidDestination(
                "destination must include a parent directory".to_string(),
            ));
        }
        let Some(raw_name) = request.destination.file_name().and_then(|n| n.to_str()) else {
            return Err(DownloadError::InvalidDestination(
                "destination must include a valid UTF-8 filename".to_string(),
            ));
        };
        let sanitized_name = sanitize_filename(raw_name);
        let dir = parent.to_path_buf();

        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled { part_path: None });
        }

        let mut cancel_open = cancel.clone();
        let mut stream = tokio::select! {
            biased;
            () = cancel_open.cancelled() => {
                return Err(DownloadError::Cancelled { part_path: None });
            }
            opened = self.fetcher.open_stream(request.url) => {
                opened.map_err(|source| DownloadError::Provider { source, part_path: None })?
            }
        };

        let total_bytes = request
            .expected_file_size
            .or_else(|| stream.content_length());
        let part_path = dir.join(format!(".{sanitized_name}.{}.part", uuid::Uuid::new_v4()));

        let create_fs = Arc::clone(&self.fs);
        let create_path = part_path.clone();
        let writer = tokio::task::spawn_blocking(move || create_fs.create_part(&create_path))
            .await
            .map_err(|err| DownloadError::Io {
                detail: format!("failed to join part-file creation: {err}"),
                part_path: None,
            })?
            .map_err(|err| map_write_io_error(err, None))?;
        let mut writer = Some(writer);

        let mut bytes_written: u64 = 0;
        let mut cancel_stream = cancel.clone();
        loop {
            let next = tokio::select! {
                biased;
                () = cancel_stream.cancelled() => {
                    drop(writer.take());
                    return Err(DownloadError::Cancelled { part_path: Some(part_path) });
                }
                chunk = stream.next_chunk() => chunk,
            };

            match next {
                Ok(Some(chunk)) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    let chunk_len = chunk.len() as u64;
                    let projected = match bytes_written.checked_add(chunk_len) {
                        Some(total) => total,
                        None => {
                            drop(writer.take());
                            remove_part_best_effort(&self.fs, &part_path).await;
                            return Err(DownloadError::SizeMismatch {
                                expected: request.expected_file_size.unwrap_or(u64::MAX),
                                actual: u64::MAX,
                            });
                        }
                    };
                    if let Some(expected) = request.expected_file_size
                        && projected > expected
                    {
                        drop(writer.take());
                        remove_part_best_effort(&self.fs, &part_path).await;
                        return Err(DownloadError::SizeMismatch {
                            expected,
                            actual: projected,
                        });
                    }

                    let mut active_writer = writer.take().expect("writer must be present");
                    let written = tokio::task::spawn_blocking(move || {
                        active_writer.write_all(&chunk)?;
                        Ok::<Box<dyn PartWriter>, std::io::Error>(active_writer)
                    })
                    .await
                    .map_err(|err| DownloadError::Io {
                        detail: format!("failed to join part-file write: {err}"),
                        part_path: Some(part_path.clone()),
                    })?
                    .map_err(|err| map_write_io_error(err, Some(part_path.clone())))?;
                    writer = Some(written);

                    bytes_written = projected;
                    if let Some(tx) = &progress {
                        let _ = tx.send(DownloadProgress {
                            bytes_written,
                            total_bytes,
                        });
                    }
                }
                Ok(None) => break,
                Err(source) => {
                    drop(writer.take());
                    return Err(DownloadError::Provider {
                        source,
                        part_path: Some(part_path),
                    });
                }
            }
        }

        // Release the 6-slot image semaphore immediately once the body finishes.
        drop(stream);

        let active_writer = writer.take().expect("writer must be present before finish");
        tokio::task::spawn_blocking(move || active_writer.finish())
            .await
            .map_err(|err| DownloadError::Io {
                detail: format!("failed to join part-file flush: {err}"),
                part_path: Some(part_path.clone()),
            })?
            .map_err(|err| map_write_io_error(err, Some(part_path.clone())))?;

        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled {
                part_path: Some(part_path),
            });
        }

        if let Some(expected) = request.expected_file_size
            && bytes_written != expected
        {
            remove_part_best_effort(&self.fs, &part_path).await;
            return Err(DownloadError::SizeMismatch {
                expected,
                actual: bytes_written,
            });
        }

        let mut cancel_decode = cancel.clone();
        let decoded = tokio::select! {
            biased;
            () = cancel_decode.cancelled() => {
                return Err(DownloadError::Cancelled { part_path: Some(part_path) });
            }
            res = self.image_guard.decode_file(&part_path, ImageClass::Preview) => res,
        };
        let decoded = match decoded {
            Ok(img) => img,
            Err(err) => {
                remove_part_best_effort(&self.fs, &part_path).await;
                return Err(DownloadError::InvalidImage(err));
            }
        };
        let actual_dimensions = decoded.dimensions();
        drop(decoded);

        if let Some(expected_dims) = request.expected_dimensions
            && actual_dimensions != expected_dims
        {
            remove_part_best_effort(&self.fs, &part_path).await;
            return Err(DownloadError::DimensionMismatch {
                expected: expected_dims,
                actual: actual_dimensions,
            });
        }

        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled {
                part_path: Some(part_path),
            });
        }

        let publisher = Arc::clone(&self.publisher);
        let pub_part_path = part_path.clone();
        let publish_result = tokio::task::spawn_blocking(move || {
            let (stem, ext) = split_sanitized_stem_ext(&sanitized_name);
            let initial = dir.join(&sanitized_name);
            match publisher.publish_no_clobber(&pub_part_path, &initial) {
                Ok(()) => return Ok((initial, false)),
                Err(PublishError::AlreadyExists) => {}
                Err(PublishError::Io(detail)) => {
                    return Err(DownloadError::Publish {
                        detail,
                        part_path: Some(pub_part_path),
                    });
                }
            }

            for n in 1..=MAX_COLLISION_ATTEMPTS {
                let candidate_name = numbered_collision_name(&stem, ext.as_deref(), n);
                let candidate = dir.join(candidate_name);
                match publisher.publish_no_clobber(&pub_part_path, &candidate) {
                    Ok(()) => return Ok((candidate, true)),
                    Err(PublishError::AlreadyExists) => continue,
                    Err(PublishError::Io(detail)) => {
                        return Err(DownloadError::Publish {
                            detail,
                            part_path: Some(pub_part_path),
                        });
                    }
                }
            }

            Err(DownloadError::CollisionLimitExceeded {
                part_path: Some(pub_part_path),
            })
        })
        .await
        .map_err(|err| DownloadError::Publish {
            detail: format!("failed to join file publication: {err}"),
            part_path: Some(part_path),
        })?;

        let (published_path, collision_renamed) = publish_result?;
        Ok(DownloadOutcome {
            path: published_path,
            bytes_written,
            dimensions: actual_dimensions,
            collision_renamed,
        })
    }
}
