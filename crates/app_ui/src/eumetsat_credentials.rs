//! Secret-safe persistence for a user's EUMETSAT API consumer credentials.
//!
//! The credential pair is kept in one versioned payload inside the operating
//! system's credential vault. Nothing in this module writes credentials to
//! BowEcho settings, logs a platform error, or exposes secret text through
//! `Debug`. Native keyring calls are synchronous; callers should invoke these
//! functions from a worker rather than the UI paint path.

use std::fmt;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Stable, non-empty identifiers for BowEcho's single EUMETSAT vault entry.
///
/// Changing either value would orphan credentials saved by earlier builds.
pub(crate) const EUMETSAT_VAULT_SERVICE: &str = "research.fahrenheit.bowecho";
pub(crate) const EUMETSAT_VAULT_ACCOUNT: &str = "eumetsat-api-consumer-v1";

const CREDENTIAL_SCHEMA_VERSION: u8 = 1;
const REDACTED: &str = "[REDACTED]";

/// A validated EUMETSAT consumer key and consumer secret.
///
/// The fields are private so callers must make an explicit accessor call when
/// handing them to the token request. The manual `Debug` implementation never
/// prints either value or its length.
pub(crate) struct EumetsatCredentials {
    consumer_key: String,
    consumer_secret: String,
}

impl EumetsatCredentials {
    pub(crate) fn new(
        consumer_key: &str,
        consumer_secret: &str,
    ) -> Result<Self, EumetsatCredentialError> {
        let consumer_key = consumer_key.trim();
        let consumer_secret = consumer_secret.trim();
        if consumer_key.is_empty() {
            return Err(EumetsatCredentialError::BlankConsumerKey);
        }
        if consumer_secret.is_empty() {
            return Err(EumetsatCredentialError::BlankConsumerSecret);
        }
        Ok(Self {
            consumer_key: consumer_key.to_owned(),
            consumer_secret: consumer_secret.to_owned(),
        })
    }

    pub(crate) fn consumer_key(&self) -> &str {
        &self.consumer_key
    }

    pub(crate) fn consumer_secret(&self) -> &str {
        &self.consumer_secret
    }
}

impl fmt::Debug for EumetsatCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EumetsatCredentials")
            .field("consumer_key", &Redacted)
            .field("consumer_secret", &Redacted)
            .finish()
    }
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// User-facing failures intentionally contain no platform detail or payload.
///
/// Some native-vault errors can include backend-specific diagnostic data. The
/// production backend discards that data at this boundary so a status line or
/// diagnostic report can safely display both `Debug` and `Display` forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum EumetsatCredentialError {
    #[error("EUMETSAT consumer key cannot be blank")]
    BlankConsumerKey,
    #[error("EUMETSAT consumer secret cannot be blank")]
    BlankConsumerSecret,
    #[error("saved EUMETSAT credentials are not readable; clear and enter them again")]
    InvalidStoredCredentials,
    #[error("saved EUMETSAT credentials use an unsupported format; clear and enter them again")]
    UnsupportedStoredSchema,
    #[error("BowEcho could not read the operating-system credential vault")]
    VaultReadFailed,
    #[error("BowEcho could not save credentials in the operating-system credential vault")]
    VaultWriteFailed,
    #[error("BowEcho could not delete credentials from the operating-system credential vault")]
    VaultDeleteFailed,
}

#[derive(Serialize)]
struct CredentialsPayloadRef<'a> {
    schema: u8,
    consumer_key: &'a str,
    consumer_secret: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialsPayload {
    schema: u8,
    consumer_key: String,
    consumer_secret: String,
}

fn encode_credentials(
    credentials: &EumetsatCredentials,
) -> Result<String, EumetsatCredentialError> {
    serde_json::to_string(&CredentialsPayloadRef {
        schema: CREDENTIAL_SCHEMA_VERSION,
        consumer_key: credentials.consumer_key(),
        consumer_secret: credentials.consumer_secret(),
    })
    .map_err(|_| EumetsatCredentialError::InvalidStoredCredentials)
}

fn decode_credentials(payload: &str) -> Result<EumetsatCredentials, EumetsatCredentialError> {
    let payload: CredentialsPayload = serde_json::from_str(payload)
        .map_err(|_| EumetsatCredentialError::InvalidStoredCredentials)?;
    if payload.schema != CREDENTIAL_SCHEMA_VERSION {
        return Err(EumetsatCredentialError::UnsupportedStoredSchema);
    }
    EumetsatCredentials::new(&payload.consumer_key, &payload.consumer_secret)
        .map_err(|_| EumetsatCredentialError::InvalidStoredCredentials)
}

#[derive(Clone, Copy)]
struct BackendFailure;

/// Narrow seam used by the real keyring backend and in-memory unit tests.
trait CredentialBackend {
    fn load_payload(&self) -> Result<Option<String>, BackendFailure>;
    fn save_payload(&self, payload: &str) -> Result<(), BackendFailure>;
    fn delete_payload(&self) -> Result<bool, BackendFailure>;
}

struct NativeCredentialBackend;

impl NativeCredentialBackend {
    fn entry() -> Result<keyring::Entry, BackendFailure> {
        #[cfg(any(windows, target_os = "macos", target_os = "ios", target_os = "linux"))]
        {
            keyring::Entry::new(EUMETSAT_VAULT_SERVICE, EUMETSAT_VAULT_ACCOUNT)
                .map_err(|_| BackendFailure)
        }
        // Keyring v3 falls back to its in-memory mock when no native backend
        // feature supports the target. Never mistake that test store for
        // secure persistence in a production build.
        #[cfg(not(any(windows, target_os = "macos", target_os = "ios", target_os = "linux")))]
        {
            Err(BackendFailure)
        }
    }
}

impl CredentialBackend for NativeCredentialBackend {
    fn load_payload(&self) -> Result<Option<String>, BackendFailure> {
        match Self::entry()?.get_password() {
            Ok(payload) => Ok(Some(payload)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(BackendFailure),
        }
    }

    fn save_payload(&self, payload: &str) -> Result<(), BackendFailure> {
        Self::entry()?
            .set_password(payload)
            .map_err(|_| BackendFailure)
    }

    fn delete_payload(&self) -> Result<bool, BackendFailure> {
        match Self::entry()?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(_) => Err(BackendFailure),
        }
    }
}

// The Windows credential store does not serialize concurrent calls to one
// entry. One process-wide guard also prevents load/save/delete races elsewhere.
static NATIVE_VAULT_LOCK: Mutex<()> = Mutex::new(());

/// Load BowEcho's saved EUMETSAT credential pair from the native OS vault.
pub(crate) fn load_credentials() -> Result<Option<EumetsatCredentials>, EumetsatCredentialError> {
    let _guard = NATIVE_VAULT_LOCK
        .lock()
        .map_err(|_| EumetsatCredentialError::VaultReadFailed)?;
    load_with_backend(&NativeCredentialBackend)
}

/// Save a validated EUMETSAT credential pair in the native OS vault.
pub(crate) fn save_credentials(
    credentials: &EumetsatCredentials,
) -> Result<(), EumetsatCredentialError> {
    let _guard = NATIVE_VAULT_LOCK
        .lock()
        .map_err(|_| EumetsatCredentialError::VaultWriteFailed)?;
    save_with_backend(&NativeCredentialBackend, credentials)
}

/// Delete BowEcho's EUMETSAT credential pair from the native OS vault.
///
/// Returns `true` when an entry was deleted and `false` when none existed.
pub(crate) fn delete_credentials() -> Result<bool, EumetsatCredentialError> {
    let _guard = NATIVE_VAULT_LOCK
        .lock()
        .map_err(|_| EumetsatCredentialError::VaultDeleteFailed)?;
    delete_with_backend(&NativeCredentialBackend)
}

fn load_with_backend(
    backend: &impl CredentialBackend,
) -> Result<Option<EumetsatCredentials>, EumetsatCredentialError> {
    backend
        .load_payload()
        .map_err(|_| EumetsatCredentialError::VaultReadFailed)?
        .map(|payload| decode_credentials(&payload))
        .transpose()
}

fn save_with_backend(
    backend: &impl CredentialBackend,
    credentials: &EumetsatCredentials,
) -> Result<(), EumetsatCredentialError> {
    let payload = encode_credentials(credentials)?;
    backend
        .save_payload(&payload)
        .map_err(|_| EumetsatCredentialError::VaultWriteFailed)
}

fn delete_with_backend(backend: &impl CredentialBackend) -> Result<bool, EumetsatCredentialError> {
    backend
        .delete_payload()
        .map_err(|_| EumetsatCredentialError::VaultDeleteFailed)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct MemoryBackend {
        payload: RefCell<Option<String>>,
    }

    impl CredentialBackend for MemoryBackend {
        fn load_payload(&self) -> Result<Option<String>, BackendFailure> {
            Ok(self.payload.borrow().clone())
        }

        fn save_payload(&self, payload: &str) -> Result<(), BackendFailure> {
            self.payload.replace(Some(payload.to_owned()));
            Ok(())
        }

        fn delete_payload(&self) -> Result<bool, BackendFailure> {
            Ok(self.payload.replace(None).is_some())
        }
    }

    #[test]
    fn validation_rejects_blanks_and_trims_surrounding_whitespace() {
        assert_eq!(
            EumetsatCredentials::new("  ", "secret").unwrap_err(),
            EumetsatCredentialError::BlankConsumerKey
        );
        assert_eq!(
            EumetsatCredentials::new("key", "\t\r\n").unwrap_err(),
            EumetsatCredentialError::BlankConsumerSecret
        );

        let credentials = EumetsatCredentials::new("  sample-key  ", " sample-secret\n")
            .expect("valid credentials");
        assert_eq!(credentials.consumer_key(), "sample-key");
        assert_eq!(credentials.consumer_secret(), "sample-secret");
    }

    #[test]
    fn versioned_payload_round_trips_through_an_offline_backend() {
        let backend = MemoryBackend::default();
        let credentials =
            EumetsatCredentials::new("offline-key", "offline-secret").expect("valid credentials");

        save_with_backend(&backend, &credentials).expect("save");
        let raw = backend.payload.borrow().clone().expect("stored payload");
        let json: serde_json::Value = serde_json::from_str(&raw).expect("JSON payload");
        assert_eq!(json["schema"], CREDENTIAL_SCHEMA_VERSION);

        let loaded = load_with_backend(&backend)
            .expect("load")
            .expect("credentials present");
        assert_eq!(loaded.consumer_key(), "offline-key");
        assert_eq!(loaded.consumer_secret(), "offline-secret");
        assert!(delete_with_backend(&backend).expect("delete"));
        assert!(!delete_with_backend(&backend).expect("idempotent delete"));
        assert!(load_with_backend(&backend).expect("load empty").is_none());
    }

    #[test]
    fn debug_and_decode_errors_never_expose_credentials() {
        let credentials = EumetsatCredentials::new("debug-key-canary", "debug-secret-canary")
            .expect("valid credentials");
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("debug-key-canary"));
        assert!(!debug.contains("debug-secret-canary"));
        assert_eq!(debug.matches(REDACTED).count(), 2);

        let malformed = r#"{"schema":1,"consumer_key":"error-key-canary","consumer_secret":""}"#;
        let error = decode_credentials(malformed).unwrap_err();
        let rendered = format!("{error:?} / {error}");
        assert!(!rendered.contains("error-key-canary"));
        assert!(!rendered.contains("consumer_secret\":\"\""));
    }

    #[test]
    fn unknown_schema_fails_closed_without_echoing_the_payload() {
        let payload = r#"{"schema":99,"consumer_key":"schema-key-canary","consumer_secret":"schema-secret-canary"}"#;
        let error = decode_credentials(payload).unwrap_err();
        assert_eq!(error, EumetsatCredentialError::UnsupportedStoredSchema);
        let rendered = format!("{error:?} / {error}");
        assert!(!rendered.contains("schema-key-canary"));
        assert!(!rendered.contains("schema-secret-canary"));
    }

    #[test]
    fn native_entry_identifiers_are_fixed_and_nonblank() {
        assert_eq!(EUMETSAT_VAULT_SERVICE, "research.fahrenheit.bowecho");
        assert_eq!(EUMETSAT_VAULT_ACCOUNT, "eumetsat-api-consumer-v1");
    }
}
