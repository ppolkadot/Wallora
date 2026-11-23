//! Application services built on top of core domain, provider, and storage contracts.

pub mod image_guard;
pub mod session;

pub use image_guard::{
    DEFAULT_DECODED_BYTES_BUDGET, DecodedImage, ImageBudget, ImageGuard, ImageGuardConfig,
    ImageGuardError, ImageHeader, PREVIEW_MAX_PIXELS, PREVIEW_MAX_SIDE, THUMBNAIL_MAX_PIXELS,
    THUMBNAIL_MAX_SIDE, default_decode_concurrency,
};
pub use session::{
    AccountRefreshPayload, LogoutOutcome, NoticeLevel, ProxyUpdateError, RefreshOutcome,
    ScopedResponse, SessionManager, SessionNotice, SignInCommitOutcome, SignInError, SignInProbe,
    SwitchError, SwitchOutcome, evaluate_username_verification, fetch_account_refresh,
    probe_sign_in,
};
