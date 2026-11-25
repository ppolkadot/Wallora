//! Application services built on top of core domain, provider, and storage contracts.

pub mod cache;
pub mod discovery;
pub mod download;
pub mod image_guard;
pub mod image_pipeline;
pub mod session;

pub use cache::{
    CacheByteStream, CacheChunkFuture, CacheClock, CacheConfig, CacheConnectivity, CacheDirEntry,
    CacheEntryKey, CacheError, CacheFs, CacheIndex, CacheIndexSnapshot, CacheIoReadError, CacheKey,
    CacheLockError, CacheLockGuard, CacheLookup, CacheNamespace, CachePutResult,
    CacheScopeTransition, CacheSourceError, CacheStats, CacheStore, CancelToken,
    DEFAULT_CACHE_CAP_BYTES, HARD_MAX_CACHE_CAP_BYTES, NoopSessionCacheSink, PendingDeleteRecord,
    PublishedEntryRecord, RealCacheFs, SessionCacheSink, SqliteCacheIndex, StagingWriter,
    SystemCacheClock, WATCHDOG_TIMEOUT, validate_cache_cap,
};

pub use discovery::{
    ApplyOutcome, DetailUnsupportedAction, DiscoveryController, DiscoveryFilters, DiscoveryStatus,
    DiscoveryView, HandOff, OPEN_ON_WALLHAVEN_LABEL, PageRequest, PageResponse, UnsupportedFeature,
    fetch_page, selectable_purity,
};
pub use download::{
    DownloadError, DownloadFs, DownloadOutcome, DownloadProgress, DownloadRequest, FilePublisher,
    HardLinkFilePublisher, OriginalChunkFuture, OriginalChunkStream, OriginalDownloader,
    OriginalFetcher, PartWriter, PublishError, RealDownloadFs,
};
pub use image_guard::{
    DEFAULT_DECODED_BYTES_BUDGET, DecodedImage, ImageBudget, ImageGuard, ImageGuardConfig,
    ImageGuardError, ImageHeader, PREVIEW_MAX_PIXELS, PREVIEW_MAX_SIDE, THUMBNAIL_MAX_PIXELS,
    THUMBNAIL_MAX_SIDE, default_decode_concurrency,
};
pub use image_pipeline::{
    ImageFetcher, ImagePipeline, ImagePipelineError, ImageRequest, ImageSource, LoadedImage,
    load_uncached,
};
pub use session::{
    AccountRefreshPayload, CacheCapUpdateError, LogoutOutcome, NoticeLevel, ProxyUpdateError,
    RefreshOutcome, ScopedResponse, SessionManager, SessionNotice, SignInCommitOutcome,
    SignInError, SignInProbe, SwitchError, SwitchOutcome, evaluate_username_verification,
    fetch_account_refresh, probe_sign_in,
};
