use std::collections::HashMap;
use std::fmt;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Secret string wrapper that zeroizes its contents on drop and redacts Debug/Display.
/// Does not implement `Serialize` so secrets cannot be accidentally serialized.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Locally generated opaque account identifier (never the upstream username or API key).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(String);

impl AccountId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether a request or cache entry belongs to guest browsing or a signed-in account.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionKind {
    Guest,
    Account(AccountId),
}

/// Scope carried by every provider request, response, cache read, and cache write.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionScope {
    pub kind: SessionKind,
    pub generation: u64,
}

impl SessionScope {
    pub fn guest(generation: u64) -> Self {
        Self {
            kind: SessionKind::Guest,
            generation,
        }
    }

    pub fn account(id: AccountId, generation: u64) -> Self {
        Self {
            kind: SessionKind::Account(id),
            generation,
        }
    }
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential store unavailable: {0}")]
    Unavailable(String),
    #[error("credential I/O failed: {0}")]
    Io(String),
}

/// OS-backed secret storage contract (implemented in `wallora-windows` for release).
pub trait CredentialStore: Send + Sync + 'static {
    fn load(&self, account: &AccountId) -> Result<Option<SecretString>, CredentialError>;
    fn save(&self, account: &AccountId, key: &SecretString) -> Result<(), CredentialError>;
    fn delete(&self, account: &AccountId) -> Result<(), CredentialError>;
}

/// In-memory credential store for tests and guest-only provider construction.
#[derive(Debug, Default)]
pub struct InMemoryCredentialStore {
    entries: RwLock<HashMap<AccountId, SecretString>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn load(&self, account: &AccountId) -> Result<Option<SecretString>, CredentialError> {
        let guard = self
            .entries
            .read()
            .map_err(|_| CredentialError::Unavailable("lock poisoned".to_string()))?;
        Ok(guard.get(account).cloned())
    }

    fn save(&self, account: &AccountId, key: &SecretString) -> Result<(), CredentialError> {
        let mut guard = self
            .entries
            .write()
            .map_err(|_| CredentialError::Unavailable("lock poisoned".to_string()))?;
        guard.insert(account.clone(), key.clone());
        Ok(())
    }

    fn delete(&self, account: &AccountId) -> Result<(), CredentialError> {
        let mut guard = self
            .entries
            .write()
            .map_err(|_| CredentialError::Unavailable("lock poisoned".to_string()))?;
        guard.remove(account);
        Ok(())
    }
}
