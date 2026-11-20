//! "Sign in with Cloudflare" for D1 connections (`dbcore::d1::oauth`), and the account and database
//! lists the editor fills its pickers from.

use std::sync::Arc;

use crate::DbError;

/// One sign-in: open `url()` in the browser, then await `finish()` for the connection's secret
/// (store it as its password). `cancel()` gives up; `finish()` then fails with `Cancelled`.
#[derive(uniffi::Object)]
pub struct D1SignIn {
    inner: dbcore::d1::oauth::SignIn,
}

#[uniffi::export]
impl D1SignIn {
    /// Listens for Cloudflare's redirect. Cancels any other sign-in still waiting.
    #[uniffi::constructor]
    pub fn start() -> Result<Arc<Self>, DbError> {
        Ok(Arc::new(Self { inner: dbcore::d1::oauth::SignIn::start()? }))
    }

    pub fn url(&self) -> String {
        self.inner.url().to_string()
    }

    pub async fn finish(&self) -> Result<String, DbError> {
        Ok(self.inner.finish().await?)
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }
}

/// Where the app keeps connection secrets, so refreshed sign-ins are saved back (refresh tokens
/// rotate) and read again when the other dbear app refreshed first.
#[uniffi::export(with_foreign)]
pub trait D1SecretSync: Send + Sync {
    fn load(&self, connection_id: String) -> Option<String>;
    fn save(&self, connection_id: String, secret: String);
}

struct Bridge(Arc<dyn D1SecretSync>);

impl dbcore::d1::oauth::SecretSync for Bridge {
    fn load(&self, connection_id: &str) -> Option<String> {
        self.0.load(connection_id.into())
    }

    fn save(&self, connection_id: &str, secret: &str) {
        self.0.save(connection_id.into(), secret.into())
    }
}

/// Installs the app's secret store (once, at launch).
#[uniffi::export]
pub fn d1_set_secret_sync(sync: Arc<dyn D1SecretSync>) {
    dbcore::d1::oauth::set_secret_sync(Arc::new(Bridge(sync)));
}

/// True when a D1 connection's password is a sign-in (not an API token).
#[uniffi::export]
pub fn d1_is_sign_in(secret: String) -> bool {
    dbcore::d1::oauth::is_secret(&secret)
}

/// The newest copy of a sign-in secret (after refreshes); save this one.
#[uniffi::export]
pub fn d1_latest_secret(secret: String) -> String {
    dbcore::d1::oauth::latest_secret(&secret)
}

/// Revokes a sign-in (signing out). Best effort.
#[uniffi::export]
pub async fn d1_revoke(secret: String) -> Result<(), DbError> {
    Ok(dbcore::d1::oauth::revoke(secret).await?)
}

#[derive(uniffi::Record)]
pub struct D1Account {
    pub id: String,
    pub name: String,
}

/// The accounts an API token or sign-in reaches.
#[uniffi::export]
pub async fn d1_accounts(secret: String) -> Result<Vec<D1Account>, DbError> {
    Ok(dbcore::d1::accounts(secret).await?.into_iter().map(|a| D1Account { id: a.id, name: a.name }).collect())
}

/// The D1 databases of `account` (empty: the token's only account), by name.
#[uniffi::export]
pub async fn d1_databases(secret: String, account: String) -> Result<Vec<String>, DbError> {
    Ok(dbcore::d1::databases(secret, account).await?)
}
