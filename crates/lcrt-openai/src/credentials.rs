//! The user's OpenAI API key: a redacted value type and its storage policy.
//!
//! The key is persisted only in the OS secret store (Secret Service on
//! Ubuntu). When no secret store is reachable it is kept in memory for the
//! current session and never written anywhere else.

use std::{error::Error, fmt};

use tracing::{info, warn};

const KEYRING_SERVICE: &str = "io.github.hoangnguyen7474.Lcrt";
const KEYRING_ACCOUNT: &str = "openai-api-key";
/// Environment variable accepted as a developer fallback.
pub const API_KEY_ENVIRONMENT_VARIABLE: &str = "OPENAI_API_KEY";
const MAX_API_KEY_BYTES: usize = 512;

/// An OpenAI API key. Its `Debug` output is redacted and it has no `Display`.
#[derive(Clone, Eq, PartialEq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Accepts a key as entered, trimming surrounding whitespace.
    ///
    /// Only structure is checked here; whether the service accepts the key
    /// is established by testing the connection, not by its prefix.
    pub fn parse(value: &str) -> Result<Self, InvalidApiKey> {
        let value = value.trim();
        if value.is_empty() {
            return Err(InvalidApiKey::Empty);
        }
        if value.len() > MAX_API_KEY_BYTES
            || value
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(InvalidApiKey::Malformed);
        }
        Ok(Self(value.to_owned()))
    }

    /// The secret itself, for the `Authorization` header only.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(<redacted>)")
    }
}

/// An API key that cannot be used as entered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidApiKey {
    /// Nothing was entered.
    Empty,
    /// The value contains whitespace or control characters, or is implausibly long.
    Malformed,
}

impl fmt::Display for InvalidApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "Enter an OpenAI API key.",
            Self::Malformed => "That doesn't look like an API key. Paste it again without spaces.",
        })
    }
}

impl Error for InvalidApiKey {}

/// Where the key in use came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialSource {
    /// Entered in this session and not persisted.
    Session,
    /// Loaded from the OS secret store.
    SecretStore,
    /// Read from `OPENAI_API_KEY`.
    Environment,
}

/// Failure of the OS secret store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretStoreError {
    /// No secret store service is reachable (for example, no keyring daemon).
    Unavailable,
    /// The store is reachable but refused or failed the operation.
    Failed(String),
}

impl fmt::Display for SecretStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("the system keyring is not available"),
            Self::Failed(message) => write!(formatter, "the system keyring failed: {message}"),
        }
    }
}

impl Error for SecretStoreError {}

/// Persistent storage for exactly one API key.
pub trait SecretStore: Send {
    /// Loads the stored key, if any.
    fn load(&self) -> Result<Option<ApiKey>, SecretStoreError>;
    /// Replaces the stored key.
    fn save(&self, key: &ApiKey) -> Result<(), SecretStoreError>;
    /// Removes the stored key; removing a missing key succeeds.
    fn clear(&self) -> Result<(), SecretStoreError>;
}

/// The user's Secret Service keyring.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyringStore;

impl KeyringStore {
    fn entry() -> Result<keyring::Entry, SecretStoreError> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).map_err(map_keyring_error)
    }
}

impl SecretStore for KeyringStore {
    fn load(&self) -> Result<Option<ApiKey>, SecretStoreError> {
        match Self::entry()?.get_password() {
            Ok(value) => Ok(ApiKey::parse(&value).ok()),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(map_keyring_error(error)),
        }
    }

    fn save(&self, key: &ApiKey) -> Result<(), SecretStoreError> {
        Self::entry()?
            .set_password(key.expose())
            .map_err(map_keyring_error)
    }

    fn clear(&self) -> Result<(), SecretStoreError> {
        match Self::entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(map_keyring_error(error)),
        }
    }
}

fn map_keyring_error(error: keyring::Error) -> SecretStoreError {
    match error {
        keyring::Error::PlatformFailure(_) | keyring::Error::NoStorageAccess(_) => {
            SecretStoreError::Unavailable
        }
        // Keyring errors describe the store, never the secret itself.
        other => SecretStoreError::Failed(other.to_string()),
    }
}

/// What the Preferences window shows about the credential.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialStatus {
    /// No key is available from any source.
    NotConfigured,
    /// A key is saved in the system keyring.
    SavedSecurely,
    /// A key was entered but the keyring is unavailable, so it lasts only
    /// until LCRT quits.
    SessionOnly,
    /// `OPENAI_API_KEY` supplies the key; its value is never shown.
    UsingEnvironment,
}

impl CredentialStatus {
    /// User-facing status text.
    pub fn label(self) -> &'static str {
        match self {
            Self::NotConfigured => "Not configured",
            Self::SavedSecurely => "Saved securely",
            Self::SessionOnly => "Saved for this session only (system keyring unavailable)",
            Self::UsingEnvironment => "Using environment credential",
        }
    }
}

/// Resolves the key in use with precedence session, secret store, environment.
pub struct Credentials<S> {
    store: S,
    session: Option<ApiKey>,
    environment: Option<ApiKey>,
}

impl<S: SecretStore> Credentials<S> {
    /// Creates the resolver; `environment` is the value of `OPENAI_API_KEY`.
    pub fn new(store: S, environment: Option<&str>) -> Self {
        Self {
            store,
            session: None,
            environment: environment.and_then(|value| ApiKey::parse(value).ok()),
        }
    }

    /// The key to use now and where it came from.
    pub fn resolve(&self) -> Option<(ApiKey, CredentialSource)> {
        if let Some(key) = &self.session {
            return Some((key.clone(), CredentialSource::Session));
        }
        match self.store.load() {
            Ok(Some(key)) => return Some((key, CredentialSource::SecretStore)),
            Ok(None) => {}
            Err(error) => warn!(%error, "could not read the API key from the keyring"),
        }
        self.environment
            .clone()
            .map(|key| (key, CredentialSource::Environment))
    }

    /// The status to display, without revealing any key.
    pub fn status(&self) -> CredentialStatus {
        match self.resolve().map(|(_, source)| source) {
            None => CredentialStatus::NotConfigured,
            Some(CredentialSource::Session) => CredentialStatus::SessionOnly,
            Some(CredentialSource::SecretStore) => CredentialStatus::SavedSecurely,
            Some(CredentialSource::Environment) => CredentialStatus::UsingEnvironment,
        }
    }

    /// Saves `key` in the keyring, or for this session only when the keyring
    /// is unavailable. It is never written in plaintext.
    pub fn save(&mut self, key: ApiKey) -> Result<CredentialStatus, SecretStoreError> {
        match self.store.save(&key) {
            Ok(()) => {
                self.session = None;
                info!("API key saved in the system keyring");
                Ok(CredentialStatus::SavedSecurely)
            }
            Err(SecretStoreError::Unavailable) => {
                self.session = Some(key);
                info!("system keyring unavailable; API key kept for this session only");
                Ok(CredentialStatus::SessionOnly)
            }
            Err(error) => Err(error),
        }
    }

    /// Forgets the session key and removes the stored key. The environment
    /// variable, if set, remains in effect. An unavailable keyring is an
    /// error: a key saved there earlier may still be stored.
    pub fn clear(&mut self) -> Result<CredentialStatus, SecretStoreError> {
        self.session = None;
        self.store.clear()?;
        Ok(self.status())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use super::{
        ApiKey, CredentialSource, CredentialStatus, Credentials, InvalidApiKey, SecretStore,
        SecretStoreError,
    };

    /// An in-memory secret store whose availability can be switched off.
    #[derive(Clone, Default)]
    pub(crate) struct FakeStore {
        pub(crate) value: Arc<Mutex<Option<ApiKey>>>,
        pub(crate) unavailable: bool,
    }

    impl SecretStore for FakeStore {
        fn load(&self) -> Result<Option<ApiKey>, SecretStoreError> {
            if self.unavailable {
                return Err(SecretStoreError::Unavailable);
            }
            Ok(self.value.lock().unwrap().clone())
        }

        fn save(&self, key: &ApiKey) -> Result<(), SecretStoreError> {
            if self.unavailable {
                return Err(SecretStoreError::Unavailable);
            }
            *self.value.lock().unwrap() = Some(key.clone());
            Ok(())
        }

        fn clear(&self) -> Result<(), SecretStoreError> {
            if self.unavailable {
                return Err(SecretStoreError::Unavailable);
            }
            *self.value.lock().unwrap() = None;
            Ok(())
        }
    }

    #[test]
    fn debug_output_never_reveals_the_key() {
        let key = ApiKey::parse("  sk-test-secret-value  ").unwrap();
        let rendered = format!("{key:?} {:?}", Some(&key));
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn keys_are_trimmed_and_structurally_validated_without_prefix_checks() {
        assert_eq!(ApiKey::parse(" \n").unwrap_err(), InvalidApiKey::Empty);
        assert_eq!(ApiKey::parse("a b").unwrap_err(), InvalidApiKey::Malformed);
        assert!(ApiKey::parse("new-format-key-without-sk-prefix").is_ok());
    }

    #[test]
    fn precedence_is_session_then_store_then_environment() {
        let store = FakeStore::default();
        let mut credentials = Credentials::new(store.clone(), Some("env-key"));
        assert_eq!(credentials.status(), CredentialStatus::UsingEnvironment);

        *store.value.lock().unwrap() = Some(ApiKey::parse("stored-key").unwrap());
        assert_eq!(
            credentials.resolve().map(|(_, source)| source),
            Some(CredentialSource::SecretStore)
        );

        credentials.session = Some(ApiKey::parse("session-key").unwrap());
        let (key, source) = credentials.resolve().unwrap();
        assert_eq!(source, CredentialSource::Session);
        assert_eq!(key, ApiKey::parse("session-key").unwrap());
    }

    #[test]
    fn saving_uses_the_keyring_when_available() {
        let store = FakeStore::default();
        let mut credentials = Credentials::new(store.clone(), None);
        assert_eq!(credentials.status(), CredentialStatus::NotConfigured);

        let status = credentials.save(ApiKey::parse("k1").unwrap()).unwrap();
        assert_eq!(status, CredentialStatus::SavedSecurely);
        assert_eq!(
            *store.value.lock().unwrap(),
            Some(ApiKey::parse("k1").unwrap())
        );
    }

    #[test]
    fn unavailable_keyring_falls_back_to_session_memory_only() {
        let store = FakeStore {
            unavailable: true,
            ..FakeStore::default()
        };
        let mut credentials = Credentials::new(store.clone(), None);

        let status = credentials.save(ApiKey::parse("k2").unwrap()).unwrap();
        assert_eq!(status, CredentialStatus::SessionOnly);
        assert_eq!(credentials.status(), CredentialStatus::SessionOnly);
        assert_eq!(*store.value.lock().unwrap(), None);
    }

    #[test]
    fn clearing_with_the_keyring_unavailable_is_reported_not_claimed() {
        let store = FakeStore {
            value: Arc::new(Mutex::new(Some(ApiKey::parse("saved").unwrap()))),
            unavailable: true,
        };
        let mut credentials = Credentials::new(store.clone(), None);
        assert_eq!(credentials.clear(), Err(SecretStoreError::Unavailable));
        assert!(store.value.lock().unwrap().is_some());
    }

    #[test]
    fn clearing_removes_stored_and_session_keys_but_not_environment() {
        let store = FakeStore::default();
        let mut credentials = Credentials::new(store.clone(), Some("env-key"));
        credentials.save(ApiKey::parse("k3").unwrap()).unwrap();

        assert_eq!(
            credentials.clear().unwrap(),
            CredentialStatus::UsingEnvironment
        );
        assert_eq!(*store.value.lock().unwrap(), None);
    }
}
