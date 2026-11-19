use std::fmt;

use wallora_core::domain::AccountId;
use wallora_core::services::{AccountRefreshPayload, SignInError, SignInProbe};

/// Top-level navigation destinations in the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Home,
    Search,
    Account,
    Settings,
}

impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Search => "Search",
            Self::Account => "Account",
            Self::Settings => "Settings",
        }
    }
}

/// UI selection state for the local proxy preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyModeChoice {
    #[default]
    System,
    Direct,
    Manual,
}

impl ProxyModeChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System proxy",
            Self::Direct => "Direct (no proxy)",
            Self::Manual => "Manual proxy",
        }
    }
}

#[derive(Clone)]
pub enum Message {
    Navigate(Route),
    UsernameChanged(String),
    ApiKeyChanged(String),
    SubmitSignIn,
    SignInProbeFinished {
        op_id: u64,
        result: Result<SignInProbe, SignInError>,
    },
    RefreshAccount {
        reverify: bool,
    },
    AccountRefreshFinished(AccountRefreshPayload),
    SignOut,
    SwitchAccount(AccountId),
    SwitchToGuest,
    ProxyModeSelected(ProxyModeChoice),
    ProxyUrlChanged(String),
    ApplyProxy,
    OpenUpstreamSettings,
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Navigate(route) => f.debug_tuple("Navigate").field(route).finish(),
            Self::UsernameChanged(u) => f.debug_tuple("UsernameChanged").field(u).finish(),
            // Never log raw API key input.
            Self::ApiKeyChanged(_) => f.write_str("ApiKeyChanged(\"***\")"),
            Self::SubmitSignIn => f.write_str("SubmitSignIn"),
            Self::SignInProbeFinished { op_id, result } => f
                .debug_struct("SignInProbeFinished")
                .field("op_id", op_id)
                .field("result", result)
                .finish(),
            Self::RefreshAccount { reverify } => f
                .debug_struct("RefreshAccount")
                .field("reverify", reverify)
                .finish(),
            Self::AccountRefreshFinished(payload) => f
                .debug_tuple("AccountRefreshFinished")
                .field(payload)
                .finish(),
            Self::SignOut => f.write_str("SignOut"),
            Self::SwitchAccount(id) => f.debug_tuple("SwitchAccount").field(id).finish(),
            Self::SwitchToGuest => f.write_str("SwitchToGuest"),
            Self::ProxyModeSelected(mode) => {
                f.debug_tuple("ProxyModeSelected").field(mode).finish()
            }
            Self::ProxyUrlChanged(url) => f.debug_tuple("ProxyUrlChanged").field(url).finish(),
            Self::ApplyProxy => f.write_str("ApplyProxy"),
            Self::OpenUpstreamSettings => f.write_str("OpenUpstreamSettings"),
        }
    }
}
