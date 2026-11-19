//! Windows Credential Manager implementation of [`CredentialStore`] backed by `keyring` 3
//! (`windows-native`).

use wallora_core::domain::{AccountId, CredentialError, CredentialStore, SecretString};

/// Default service namespace used in Windows Credential Manager Generic Credentials.
pub const DEFAULT_SERVICE_NAME: &str = "Wallora";

/// OS-backed credential store using Windows Credential Manager via `keyring` (`windows-native`).
#[derive(Debug, Clone)]
pub struct WindowsCredentialStore {
    service: String,
}

impl Default for WindowsCredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsCredentialStore {
    pub fn new() -> Self {
        Self {
            service: DEFAULT_SERVICE_NAME.to_string(),
        }
    }

    /// Creates a store using a custom service namespace (useful for isolated smoke tests).
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    pub fn service_name(&self) -> &str {
        &self.service
    }

    fn entry(&self, account: &AccountId) -> Result<keyring::Entry, CredentialError> {
        if account.as_str().trim().is_empty() {
            return Err(CredentialError::Io("empty account identifier".to_string()));
        }
        keyring::Entry::new(&self.service, account.as_str()).map_err(map_keyring_error)
    }
}

fn map_keyring_error(err: keyring::Error) -> CredentialError {
    match err {
        keyring::Error::NoStorageAccess(_) => {
            CredentialError::Unavailable("Windows Credential Manager access denied".to_string())
        }
        keyring::Error::PlatformFailure(_) => {
            CredentialError::Unavailable("Windows Credential Manager platform error".to_string())
        }
        keyring::Error::BadEncoding(_) => {
            CredentialError::Io("stored credential had invalid UTF-8 encoding".to_string())
        }
        keyring::Error::TooLong(field, max) => {
            CredentialError::Io(format!("credential {field} exceeded {max} byte limit"))
        }
        keyring::Error::Invalid(field, _) => {
            CredentialError::Io(format!("invalid credential {field}"))
        }
        keyring::Error::Ambiguous(_) => {
            CredentialError::Io("multiple matching credential entries found".to_string())
        }
        keyring::Error::NoEntry => CredentialError::Io("credential entry not found".to_string()),
        _ => CredentialError::Unavailable("unexpected credential store error".to_string()),
    }
}

impl CredentialStore for WindowsCredentialStore {
    fn load(&self, account: &AccountId) -> Result<Option<SecretString>, CredentialError> {
        let entry = self.entry(account)?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(SecretString::new(secret))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(map_keyring_error(err)),
        }
    }

    fn save(&self, account: &AccountId, key: &SecretString) -> Result<(), CredentialError> {
        if key.is_empty() {
            return Err(CredentialError::Io(
                "cannot save an empty credential".to_string(),
            ));
        }
        let entry = self.entry(account)?;
        entry
            .set_password(key.expose_secret())
            .map_err(map_keyring_error)
    }

    fn delete(&self, account: &AccountId) -> Result<(), CredentialError> {
        let entry = self.entry(account)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(map_keyring_error(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScopedCredentialCleanup<'a> {
        store: &'a WindowsCredentialStore,
        account: &'a AccountId,
    }

    impl Drop for ScopedCredentialCleanup<'_> {
        fn drop(&mut self) {
            let _ = self.store.delete(self.account);
        }
    }

    #[test]
    fn rejects_empty_account_id_without_touching_os_store() {
        let store = WindowsCredentialStore::with_service("Wallora.UnitTest.Unused");
        let empty = AccountId::new("   ");
        assert!(store.load(&empty).is_err());
        assert!(store.save(&empty, &SecretString::new("synthetic")).is_err());
        assert!(store.delete(&empty).is_err());
    }

    #[test]
    fn rejects_empty_secret_before_os_write() {
        let store = WindowsCredentialStore::with_service("Wallora.UnitTest.Unused");
        let account = AccountId::new("unit-test-account");
        let err = store
            .save(&account, &SecretString::new(""))
            .expect_err("empty secret must fail");
        assert!(err.to_string().contains("empty credential"));
    }

    /// Manual smoke test against the real Windows Credential Manager using a unique synthetic
    /// target with guaranteed RAII cleanup on drop.
    ///
    /// Run explicitly via:
    /// `cargo test --locked -p wallora-windows -- --ignored --nocapture`
    #[test]
    #[ignore = "touches real Windows Credential Manager; run explicitly with --ignored"]
    fn windows_credential_manager_smoke_write_read_delete() {
        let unique = uuid::Uuid::new_v4();
        let service = format!("Wallora.SmokeTest.{unique}");
        let account = AccountId::new(format!("smoke-account-{unique}"));
        let store = WindowsCredentialStore::with_service(&service);
        let _cleanup = ScopedCredentialCleanup {
            store: &store,
            account: &account,
        };

        // 1. Initially absent.
        assert!(
            store.load(&account).expect("initial load").is_none(),
            "fresh synthetic target should not exist"
        );

        // 2. Write synthetic secret and read it back.
        let secret1 = SecretString::new("wh_synthetic_smoke_key_alpha_0001");
        store.save(&account, &secret1).expect("save synthetic key");
        let loaded1 = store
            .load(&account)
            .expect("load saved key")
            .expect("saved key must be present");
        assert_eq!(loaded1.expose_secret(), secret1.expose_secret());
        assert_eq!(format!("{loaded1:?}"), "***");

        // 3. Overwrite with a second synthetic secret and verify update.
        let secret2 = SecretString::new("wh_synthetic_smoke_key_beta_0002");
        store
            .save(&account, &secret2)
            .expect("overwrite synthetic key");
        let loaded2 = store
            .load(&account)
            .expect("load updated key")
            .expect("updated key must be present");
        assert_eq!(loaded2.expose_secret(), secret2.expose_secret());

        // 4. Delete and verify confirmed absence + idempotent second delete.
        store.delete(&account).expect("delete synthetic key");
        assert!(
            store.load(&account).expect("load after delete").is_none(),
            "deleted key must be absent"
        );
        store
            .delete(&account)
            .expect("second delete must be idempotent");
    }
}
