//! Saving, copying, deleting, importing and testing connections: the store first, then the
//! keychain, in the one order every frontend should use.

use std::path::Path;
use std::sync::Arc;

use dbcore::import::{dbeaver, ImportScan};
use dbcore::secrets;
use dbcore::{Connection, ConnectionConfig, DatabaseKind, Error, Result, SshAuth};
use serde::{Deserialize, Serialize};

use crate::{unblock, unknown, Service, Target};

/// What an editor saves: the connection, plus the secrets typed into it.
#[derive(Clone, Deserialize)]
pub struct SaveRequest {
    /// An empty id adds a connection; another replaces the one with that id.
    pub config: ConnectionConfig,
    /// `None`: keep the saved password. `Some("")`: delete it (e.g. signed out of Cloudflare).
    /// D1 sign-ins are replaced by the newest rotation of their grant (`oauth::latest_secret`).
    #[serde(default)]
    pub password: Option<String>,
    /// The SSH password or passphrase: saved when given. Deleted whenever the connection has no
    /// tunnel or the tunnel uses the agent, kept otherwise.
    #[serde(default)]
    pub ssh_secret: Option<String>,
}

impl SaveRequest {
    pub fn new(config: ConnectionConfig) -> Self {
        Self { config, password: None, ssh_secret: None }
    }
}

/// Which keychain entry of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretKind {
    Password,
    Ssh,
}

impl SecretKind {
    /// "password", "SSH password": for "Saved, but the {label} wasn’t: …".
    pub fn label(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::Ssh => "SSH password",
        }
    }
}

/// A keychain write that failed after the connection itself was saved (nothing is rolled back).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretProblem {
    pub secret: SecretKind,
    pub error: Error,
}

impl std::fmt::Display for SecretProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Saved, but the {} wasn’t: {}", self.secret.label(), self.error)
    }
}

/// A saved connection (as stored: no secrets) and what didn't make it to the keychain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub config: ConnectionConfig,
    pub problems: Vec<SecretProblem>,
}

/// What an import added, and what went wrong ("name: why", one per line in the apps).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Import {
    pub imported: Vec<ConnectionConfig>,
    pub problems: Vec<String>,
}

fn no_keyring() -> Error {
    Error::Unsupported("no keyring".into())
}

impl Service {
    /// Saves a connection from an editor (**blocking**): upsert into the store, then the password
    /// (when given), then the SSH secret (saved when given and the tunnel isn't the agent's; deleted
    /// when there's no tunnel or it uses the agent). Keychain failures don't undo the save: they're
    /// in [`Saved::problems`]. Everything cached for the connection is forgotten.
    pub fn save(&self, request: SaveRequest) -> Result<Saved> {
        let SaveRequest { config, mut password, ssh_secret } = request;
        if config.kind == DatabaseKind::D1 {
            password = password.map(|p| if p.is_empty() { p } else { dbcore::d1::oauth::latest_secret(&p) });
        }
        let saved = {
            let mut store = self.store()?;
            let saved = store.upsert(config)?;
            self.publish(&store);
            saved
        };
        let mut problems = Vec::new();
        if let Some(password) = password {
            let result = match &self.inner.secrets {
                Some(store) => secrets::save_password(store.as_ref(), &saved.id, Some(&password)),
                None => Err(no_keyring()),
            };
            if let Err(error) = result {
                problems.push(SecretProblem { secret: SecretKind::Password, error });
            }
        }
        let ssh_account = secrets::ssh_account(&saved.id);
        let keeps_ssh_secret = saved.ssh.as_ref().is_some_and(|s| s.auth != SshAuth::Agent);
        if let Some(store) = &self.inner.secrets {
            let result = match (&ssh_secret, keeps_ssh_secret) {
                (Some(secret), true) => secrets::save_password(store.as_ref(), &ssh_account, Some(secret)),
                (_, false) => secrets::save_password(store.as_ref(), &ssh_account, None),
                (None, true) => Ok(()),
            };
            if let Err(error) = result {
                problems.push(SecretProblem { secret: SecretKind::Ssh, error });
            }
        }
        self.forget(&saved.id);
        Ok(Saved { config: saved, problems })
    }

    /// Adds a copy of `config` named "… copy", with the same password and SSH secret (**blocking**).
    pub fn duplicate(&self, config: &ConnectionConfig) -> Result<Saved> {
        let mut copy = config.clone();
        copy.id = String::new();
        copy.name = format!("{} copy", config.name);
        copy.password = None;
        let saved = {
            let mut store = self.store()?;
            let saved = store.upsert(copy)?;
            self.publish(&store);
            saved
        };
        let mut problems = Vec::new();
        if let Some(store) = &self.inner.secrets {
            let entries = [
                (config.id.clone(), saved.id.clone(), SecretKind::Password),
                (secrets::ssh_account(&config.id), secrets::ssh_account(&saved.id), SecretKind::Ssh),
            ];
            for (from, to, secret) in entries {
                if let Ok(Some(value)) = store.password(&from) {
                    if let Err(error) = secrets::save_password(store.as_ref(), &to, Some(&value)) {
                        problems.push(SecretProblem { secret, error });
                    }
                }
            }
        }
        Ok(Saved { config: saved, problems })
    }

    /// Renames saved connection `id` (**blocking**): only its name changes in the store (see
    /// `ConnectionStore::rename`: trimmed, empty → its default name, duplicates allowed), so what
    /// another process changed meanwhile and its `last_database` stay. Keychain entries and history
    /// are keyed by id: nothing moves there, and nothing cached is dropped (sessions don't depend
    /// on the name). Connections opened from a URL and the samples aren't saved: refused.
    pub fn rename(&self, id: &str, name: &str) -> Result<ConnectionConfig> {
        if id.starts_with(crate::URL_ID_PREFIX) {
            return Err(Error::InvalidConfig("Connections opened from a URL aren’t saved, so they can’t be renamed.".into()));
        }
        let mut store = self.store()?;
        if store.get(id).is_none() {
            // Saved by another process since the last read?
            store.reload()?;
            if store.get(id).is_none() && self.config(id).is_some() {
                return Err(Error::InvalidConfig("Sample connections aren’t saved, so they can’t be renamed.".into()));
            }
        }
        let renamed = store.rename(id, name);
        self.publish(&store);
        renamed
    }

    /// Deletes connection `id` (**blocking**): from the store, then both keychain entries and its
    /// history (best effort), and forgets its caches. Returns whether it existed.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let removed = {
            let mut store = self.store()?;
            let removed = store.remove(id)?;
            self.publish(&store);
            removed
        };
        if let Some(store) = &self.inner.secrets {
            let _ = secrets::save_password(store.as_ref(), id, None);
            let _ = secrets::save_password(store.as_ref(), &secrets::ssh_account(id), None);
        }
        if let Some(mut state) = self.state() {
            let _ = state.clear_history(id);
        }
        self.forget(id);
        Ok(removed)
    }

    /// Finds DBeaver's connections (`path`: a data-sources.json or a folder with them; `None`: the
    /// default places), flagging those already saved. **Blocking** (reads and decrypts files).
    pub fn scan_dbeaver(&self, path: Option<&Path>) -> Result<ImportScan> {
        let scan = match path {
            Some(path) => dbeaver::scan_path(path)?,
            None => dbeaver::scan_default()?,
        };
        Ok(scan.mark_existing(&self.connections()))
    }

    /// Adds imported connections, each with its password and SSH secret moved to the keychain
    /// (**blocking**). Configs carry their imported secrets; each is added as a new connection when
    /// its id is empty. Nothing is opened.
    pub fn import(&self, configs: Vec<ConnectionConfig>) -> Result<Import> {
        let mut store = self.store()?;
        let mut outcome = Import::default();
        for mut config in configs {
            let password = config.password.take().filter(|p| !p.is_empty());
            let ssh_secret = config.ssh.as_mut().and_then(|s| s.secret.take()).filter(|p| !p.is_empty());
            let name = config.name.clone();
            match store.upsert(config) {
                Ok(saved) => {
                    let entries = [(saved.id.clone(), password, "password"), (secrets::ssh_account(&saved.id), ssh_secret, "SSH password")];
                    for (account, secret, what) in entries {
                        let Some(secret) = secret else { continue };
                        match &self.inner.secrets {
                            Some(secrets_store) => {
                                if let Err(e) = secrets::save_password(secrets_store.as_ref(), &account, Some(&secret)) {
                                    outcome.problems.push(format!("{name}: the {what} wasn’t saved ({e})"));
                                }
                            }
                            None => outcome.problems.push(format!("{name}: the {what} wasn’t saved (no keyring)")),
                        }
                    }
                    outcome.imported.push(saved);
                }
                Err(e) => outcome.problems.push(format!("{name}: {e}")),
            }
        }
        self.publish(&store);
        Ok(outcome)
    }

    /// Connects with `config` (and its typed secrets), then disconnects. `use_saved_secrets` fills
    /// what wasn't typed from the keychain (editing a saved connection without retyping).
    pub async fn test(&self, config: ConnectionConfig, use_saved_secrets: bool) -> Result<()> {
        let config = if use_saved_secrets {
            let service = self.clone();
            unblock(move || service.fill_secrets(config)).await
        } else {
            config
        };
        let connection = Connection::new(config);
        let result = connection.connect().await;
        connection.disconnect().await;
        result
    }

    /// Connection `id` as a URL, with its saved password when asked (Copy URL). **Blocking** with a password.
    pub fn url(&self, id: &str, include_password: bool) -> Result<String> {
        let config = self.config(id).ok_or_else(|| unknown(id))?;
        let config = if include_password { self.fill_secrets(config) } else { config };
        Ok(config.to_url(include_password))
    }

    /// Creates database `name` on connection `id`'s server: over any session open on it, else a
    /// short-lived one. The cached database list is dropped so the next load lists it.
    pub async fn create_database(&self, id: &str, name: &str) -> Result<()> {
        let config = self.config(id).ok_or_else(|| unknown(id))?;
        let existing = self.open_connections().into_iter().find(|(t, _)| t.connection_id == id).map(|(_, c)| c);
        let (connection, temporary): (Arc<Connection>, bool) = match existing {
            Some(connection) => (connection, false),
            None => (self.new_connection(&Target::default_for(&config)).await?, true),
        };
        let result = connection.create_database(name.to_string()).await;
        if temporary {
            connection.disconnect().await;
        }
        result?;
        self.invalidate_databases(id);
        Ok(())
    }
}
