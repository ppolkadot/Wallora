//! Application services built on top of core domain, provider, and storage contracts.

pub mod session;

pub use session::{
    AccountRefreshPayload, LogoutOutcome, NoticeLevel, ProxyUpdateError, RefreshOutcome,
    ScopedResponse, SessionManager, SessionNotice, SignInCommitOutcome, SignInError, SignInProbe,
    SwitchError, SwitchOutcome, evaluate_username_verification, fetch_account_refresh,
    probe_sign_in,
};
