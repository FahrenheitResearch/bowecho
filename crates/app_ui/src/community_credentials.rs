//! Secret-safe persistence for the Community Cache origin bearer token.
//!
//! Non-secret endpoints and the pinned manifest public key belong in normal
//! settings. The bearer token never does: it lives only in the operating
//! system credential vault and is deliberately redacted from every error and
//! `Debug` value. Relay credentials are short-lived Phase 2 material and must
//! never be persisted through this module.

use std::fmt;
use std::sync::Mutex;

pub(crate) const COMMUNITY_VAULT_SERVICE: &str = "research.fahrenheit.bowecho";
pub(crate) const COMMUNITY_VAULT_ACCOUNT: &str = "community-cache-origin-token-v1";

const REDACTED: &str = "[REDACTED]";

/// A validated Rusty Weather origin bearer token.
pub(crate) struct CommunityOriginCredentials {
    bearer_token: String,
}

impl CommunityOriginCredentials {
    pub(crate) fn new(token: &str) -> Result<Self, CommunityCredentialError> {
        let bearer_token = token.trim();
        if bearer_token.is_empty() {
            return Err(CommunityCredentialError::BlankToken);
        }
        Ok(Self {
            bearer_token: bearer_token.to_owned(),
        })
    }

    pub(crate) fn bearer_token(&self) -> &str {
        &self.bearer_token
    }
}

impl fmt::Debug for CommunityOriginCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommunityOriginCredentials")
            .field("bearer_token", &Redacted)
            .finish()
    }
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// User-visible failures intentionally discard native-vault details because
/// platform errors can contain account or backend diagnostic data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CommunityCredentialError {
    #[error("Community Cache origin token cannot be blank")]
    BlankToken,
    #[error("BowEcho could not read the operating-system credential vault")]
    VaultReadFailed,
    #[error("BowEcho could not save the Community Cache origin token")]
    VaultWriteFailed,
    #[error("BowEcho could not delete the Community Cache origin token")]
    VaultDeleteFailed,
}

#[derive(Clone, Copy)]
struct BackendFailure;

trait CredentialBackend {
    fn load_token(&self) -> Result<Option<String>, BackendFailure>;
    fn save_token(&self, token: &str) -> Result<(), BackendFailure>;
    fn delete_token(&self) -> Result<bool, BackendFailure>;
}

struct NativeCredentialBackend;

impl NativeCredentialBackend {
    fn entry() -> Result<keyring::Entry, BackendFailure> {
        #[cfg(any(windows, target_os = "macos", target_os = "ios", target_os = "linux"))]
        {
            keyring::Entry::new(COMMUNITY_VAULT_SERVICE, COMMUNITY_VAULT_ACCOUNT)
                .map_err(|_| BackendFailure)
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "ios", target_os = "linux")))]
        {
            Err(BackendFailure)
        }
    }
}

impl CredentialBackend for NativeCredentialBackend {
    fn load_token(&self) -> Result<Option<String>, BackendFailure> {
        match Self::entry()?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(BackendFailure),
        }
    }

    fn save_token(&self, token: &str) -> Result<(), BackendFailure> {
        Self::entry()?
            .set_password(token)
            .map_err(|_| BackendFailure)
    }

    fn delete_token(&self) -> Result<bool, BackendFailure> {
        match Self::entry()?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(_) => Err(BackendFailure),
        }
    }
}

static NATIVE_VAULT_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn load_credentials()
-> Result<Option<CommunityOriginCredentials>, CommunityCredentialError> {
    let _guard = NATIVE_VAULT_LOCK
        .lock()
        .map_err(|_| CommunityCredentialError::VaultReadFailed)?;
    load_with_backend(&NativeCredentialBackend)
}

pub(crate) fn save_credentials(
    credentials: &CommunityOriginCredentials,
) -> Result<(), CommunityCredentialError> {
    let _guard = NATIVE_VAULT_LOCK
        .lock()
        .map_err(|_| CommunityCredentialError::VaultWriteFailed)?;
    save_with_backend(&NativeCredentialBackend, credentials)
}

pub(crate) fn delete_credentials() -> Result<bool, CommunityCredentialError> {
    let _guard = NATIVE_VAULT_LOCK
        .lock()
        .map_err(|_| CommunityCredentialError::VaultDeleteFailed)?;
    delete_with_backend(&NativeCredentialBackend)
}

fn load_with_backend(
    backend: &impl CredentialBackend,
) -> Result<Option<CommunityOriginCredentials>, CommunityCredentialError> {
    backend
        .load_token()
        .map_err(|_| CommunityCredentialError::VaultReadFailed)?
        .map(|token| {
            CommunityOriginCredentials::new(&token)
                .map_err(|_| CommunityCredentialError::VaultReadFailed)
        })
        .transpose()
}

fn save_with_backend(
    backend: &impl CredentialBackend,
    credentials: &CommunityOriginCredentials,
) -> Result<(), CommunityCredentialError> {
    backend
        .save_token(credentials.bearer_token())
        .map_err(|_| CommunityCredentialError::VaultWriteFailed)
}

fn delete_with_backend(backend: &impl CredentialBackend) -> Result<bool, CommunityCredentialError> {
    backend
        .delete_token()
        .map_err(|_| CommunityCredentialError::VaultDeleteFailed)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct MemoryBackend {
        token: RefCell<Option<String>>,
        fail: RefCell<bool>,
    }

    impl CredentialBackend for MemoryBackend {
        fn load_token(&self) -> Result<Option<String>, BackendFailure> {
            if *self.fail.borrow() {
                return Err(BackendFailure);
            }
            Ok(self.token.borrow().clone())
        }

        fn save_token(&self, token: &str) -> Result<(), BackendFailure> {
            if *self.fail.borrow() {
                return Err(BackendFailure);
            }
            self.token.replace(Some(token.to_owned()));
            Ok(())
        }

        fn delete_token(&self) -> Result<bool, BackendFailure> {
            if *self.fail.borrow() {
                return Err(BackendFailure);
            }
            Ok(self.token.replace(None).is_some())
        }
    }

    #[test]
    fn token_is_trimmed_round_tripped_and_deleted() {
        let backend = MemoryBackend::default();
        let credentials = CommunityOriginCredentials::new("  test-token  ").unwrap();
        assert_eq!(credentials.bearer_token(), "test-token");
        save_with_backend(&backend, &credentials).unwrap();
        assert_eq!(
            load_with_backend(&backend).unwrap().unwrap().bearer_token(),
            "test-token"
        );
        assert!(delete_with_backend(&backend).unwrap());
        assert!(!delete_with_backend(&backend).unwrap());
        assert!(load_with_backend(&backend).unwrap().is_none());
    }

    #[test]
    fn blank_and_invalid_stored_tokens_fail_closed() {
        assert_eq!(
            CommunityOriginCredentials::new(" \t\r\n").unwrap_err(),
            CommunityCredentialError::BlankToken
        );
        let backend = MemoryBackend::default();
        backend.token.replace(Some("   ".to_owned()));
        assert_eq!(
            load_with_backend(&backend).unwrap_err(),
            CommunityCredentialError::VaultReadFailed
        );
    }

    #[test]
    fn debug_and_backend_errors_do_not_expose_secret_material() {
        let credentials = CommunityOriginCredentials::new("secret-token-canary").unwrap();
        let debug = format!("{credentials:?}");
        assert!(debug.contains(REDACTED));
        assert!(!debug.contains("secret-token-canary"));

        let backend = MemoryBackend::default();
        backend.fail.replace(true);
        let error = load_with_backend(&backend).unwrap_err();
        let message = format!("{error:?} {error}");
        assert!(!message.contains("secret-token-canary"));
    }
}
