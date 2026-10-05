//! Where connection passwords live. The connection store never writes them to disk; frontends keep
//! them in the OS credential store, one entry per connection id.
//!
//! - [`MemorySecretStore`]: tests and previews.
//! - [`KeyringSecretStore`] (feature `os-keyring`): the macOS Keychain, the Secret Service on Linux
//!   (GNOME Keyring, KWallet, KeePassXC…) or the Windows Credential Manager. Entries use the same
//!   service and account as the macOS app, so every frontend on a machine shares them.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::driver::Result;
use crate::model::ConnectionConfig;

/// Service name of every password entry (the account is the connection id).
pub const SERVICE: &str = "ar.fausto.dbear.connection";
/// Services used by earlier builds. Passwords found there move to [`SERVICE`] the first time they're read.
pub const LEGACY_SERVICES: &[&str] = &["dev.fausto.dbear.connection", "dev.fausto.dbgui.connection"];

pub trait SecretStore: Send + Sync {
    /// The saved password, if any. May show an OS prompt (e.g. Keychain access, unlocking a keyring).
    fn password(&self, id: &str) -> Result<Option<String>>;
    fn set_password(&self, id: &str, password: &str) -> Result<()>;
    /// Removes the password. Not an error when there is none.
    fn delete_password(&self, id: &str) -> Result<()>;
}

/// `config` with its saved password filled in, unless it already carries one (typed into a form).
pub fn with_password(store: &dyn SecretStore, mut config: ConnectionConfig) -> Result<ConnectionConfig> {
    if config.password.is_none() {
        config.password = store.password(&config.id)?;
    }
    Ok(config)
}

/// Saves `password` for `id`, or forgets the saved one when it's `None` or empty.
pub fn save_password(store: &dyn SecretStore, id: &str, password: Option<&str>) -> Result<()> {
    match password.filter(|p| !p.is_empty()) {
        Some(password) => store.set_password(id, password),
        None => store.delete_password(id),
    }
}

/// Passwords kept in memory only.
#[derive(Default)]
pub struct MemorySecretStore {
    values: Mutex<HashMap<String, String>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn values(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.values.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SecretStore for MemorySecretStore {
    fn password(&self, id: &str) -> Result<Option<String>> {
        Ok(self.values().get(id).cloned())
    }

    fn set_password(&self, id: &str, password: &str) -> Result<()> {
        self.values().insert(id.into(), password.into());
        Ok(())
    }

    fn delete_password(&self, id: &str) -> Result<()> {
        self.values().remove(id);
        Ok(())
    }
}

#[cfg(feature = "os-keyring")]
pub use os::KeyringSecretStore;

#[cfg(feature = "os-keyring")]
mod os {
    use std::sync::Arc;

    use keyring_core::{CredentialStore, Entry, Error as KeyringError};

    use super::{LEGACY_SERVICES, SERVICE, SecretStore};
    use crate::driver::{Error, Result};

    /// The OS credential store.
    pub struct KeyringSecretStore {
        store: Arc<CredentialStore>,
        service: String,
        legacy: Vec<String>,
    }

    impl KeyringSecretStore {
        /// Opens the platform's store. Fails on Linux when no Secret Service is running.
        pub fn new() -> Result<Self> {
            Self::with_service(SERVICE, LEGACY_SERVICES)
        }

        /// A store under another service name (tests use a scratch one).
        pub fn with_service(service: &str, legacy: &[&str]) -> Result<Self> {
            Ok(Self {
                store: platform_store()?,
                service: service.into(),
                legacy: legacy.iter().map(|s| s.to_string()).collect(),
            })
        }

        /// What the user knows the store as, for messages.
        pub fn name() -> &'static str {
            if cfg!(target_os = "macos") {
                "Keychain"
            } else if cfg!(windows) {
                "Credential Manager"
            } else {
                "Secret Service"
            }
        }

        fn entry(&self, service: &str, id: &str) -> Result<Entry> {
            self.store.build(service, id, None).map_err(failure)
        }

        fn read(&self, service: &str, id: &str) -> Result<Option<String>> {
            match self.entry(service, id)?.get_password() {
                Ok(password) => Ok(Some(password)),
                Err(KeyringError::NoEntry) => Ok(None),
                Err(e) => Err(failure(e)),
            }
        }

        fn remove(&self, service: &str, id: &str) -> Result<()> {
            match self.entry(service, id)?.delete_credential() {
                Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
                Err(e) => Err(failure(e)),
            }
        }
    }

    impl SecretStore for KeyringSecretStore {
        fn password(&self, id: &str) -> Result<Option<String>> {
            if let Some(password) = self.read(&self.service, id)? {
                return Ok(Some(password));
            }
            for legacy in &self.legacy {
                // Best effort: an unreadable legacy entry just means no password.
                let Ok(Some(password)) = self.read(legacy, id) else { continue };
                if self.set_password(id, &password).is_ok() {
                    let _ = self.remove(legacy, id);
                }
                return Ok(Some(password));
            }
            Ok(None)
        }

        fn set_password(&self, id: &str, password: &str) -> Result<()> {
            self.entry(&self.service, id)?.set_password(password).map_err(failure)
        }

        fn delete_password(&self, id: &str) -> Result<()> {
            self.remove(&self.service, id)?;
            for legacy in &self.legacy {
                let _ = self.remove(legacy, id);
            }
            Ok(())
        }
    }

    fn failure(e: KeyringError) -> Error {
        Error::Storage(format!("{}: {e}", KeyringSecretStore::name()))
    }

    fn platform_store() -> Result<Arc<CredentialStore>> {
        #[cfg(target_os = "macos")]
        let store: Arc<CredentialStore> = apple_native_keyring_store::keychain::Store::new().map_err(failure)?;
        #[cfg(windows)]
        let store: Arc<CredentialStore> = windows_native_keyring_store::Store::new().map_err(failure)?;
        #[cfg(all(unix, not(target_os = "macos")))]
        let store: Arc<CredentialStore> = zbus_secret_service_keyring_store::Store::new().map_err(failure)?;
        Ok(store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DatabaseKind, SslMode};

    fn config(id: &str, password: Option<&str>) -> ConnectionConfig {
        ConnectionConfig {
            id: id.into(),
            name: "app".into(),
            group: String::new(),
            kind: DatabaseKind::Postgres,
            host: "localhost".into(),
            port: Some(5432),
            database: "app".into(),
            user: Some("app".into()),
            password: password.map(Into::into),
            ssl_mode: SslMode::default(),
            show_all_databases: true,
        }
    }

    #[test]
    fn memory_store_round_trip() {
        let store = MemorySecretStore::new();
        assert_eq!(store.password("a").unwrap(), None);
        store.set_password("a", "hunter2").unwrap();
        assert_eq!(store.password("a").unwrap().as_deref(), Some("hunter2"));
        store.delete_password("a").unwrap();
        store.delete_password("a").unwrap();
        assert_eq!(store.password("a").unwrap(), None);
    }

    #[test]
    fn with_password_keeps_a_typed_password() {
        let store = MemorySecretStore::new();
        store.set_password("a", "saved").unwrap();
        let filled = with_password(&store, config("a", None)).unwrap();
        assert_eq!(filled.password.as_deref(), Some("saved"));
        let typed = with_password(&store, config("a", Some("typed"))).unwrap();
        assert_eq!(typed.password.as_deref(), Some("typed"));
        assert_eq!(with_password(&store, config("b", None)).unwrap().password, None);
    }

    #[test]
    fn save_password_forgets_empty_passwords() {
        let store = MemorySecretStore::new();
        save_password(&store, "a", Some("x")).unwrap();
        assert_eq!(store.password("a").unwrap().as_deref(), Some("x"));
        save_password(&store, "a", Some("")).unwrap();
        assert_eq!(store.password("a").unwrap(), None);
        save_password(&store, "a", Some("y")).unwrap();
        save_password(&store, "a", None).unwrap();
        assert_eq!(store.password("a").unwrap(), None);
    }

    /// Writes to the real OS store under a scratch service; only runs with `DBEAR_TEST_KEYRING=1`.
    #[cfg(feature = "os-keyring")]
    #[test]
    fn os_store_round_trip_and_legacy_move() {
        if std::env::var("DBEAR_TEST_KEYRING").as_deref() != Ok("1") {
            return;
        }
        let tag = uuid::Uuid::new_v4();
        let (service, legacy) = (format!("ar.fausto.dbear.test-{tag}"), format!("ar.fausto.dbear.test-legacy-{tag}"));
        let store = KeyringSecretStore::with_service(&service, &[legacy.as_str()]).unwrap();
        let old = KeyringSecretStore::with_service(&legacy, &[]).unwrap();
        let id = uuid::Uuid::new_v4().to_string();

        assert_eq!(store.password(&id).unwrap(), None);
        store.set_password(&id, "pässwörd").unwrap();
        assert_eq!(store.password(&id).unwrap().as_deref(), Some("pässwörd"));
        store.set_password(&id, "changed").unwrap();
        assert_eq!(store.password(&id).unwrap().as_deref(), Some("changed"));
        store.delete_password(&id).unwrap();
        store.delete_password(&id).unwrap();
        assert_eq!(store.password(&id).unwrap(), None);

        // A password saved under the legacy service is moved on first read.
        old.set_password(&id, "from-legacy").unwrap();
        assert_eq!(store.password(&id).unwrap().as_deref(), Some("from-legacy"));
        assert_eq!(old.password(&id).unwrap(), None);
        store.delete_password(&id).unwrap();
        assert_eq!(store.password(&id).unwrap(), None);
    }
}
