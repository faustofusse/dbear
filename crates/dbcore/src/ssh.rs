//! SSH tunnels: the database is reached through an SSH server, like `ssh -L`.
//!
//! [`Tunnel::open`] signs in to the SSH server and listens on a free local port; each connection to
//! it is forwarded to the database (`direct-tcpip`). Drivers then connect to `127.0.0.1:<port>`
//! but keep the database's host name for TLS ([`ConnectionConfig::tunneled_port`]).
//! [`TunneledDriver`] opens the tunnel on first use, and again if the SSH server dropped it.
//!
//! Host keys: a key that doesn't match the user's `~/.ssh/known_hosts` is refused. A host that
//! isn't listed there is trusted the first time and remembered in dbear's own `known_hosts` (in the
//! app's data folder; the user's file is never written), and refused if its key changes later.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::keys::{self, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::access::{DatabaseAccess, DatabaseLevelContext, Grant, Role, RoleRef};
use crate::driver::{Driver, Error, Result};
use crate::edit::EditStatement;
use crate::keyset::{PageCursor, RowPage};
use crate::model::{ConnectionConfig, QueryResult, RowQuery, Schema, SshAuth, SshTunnel, TableColumns, TableInfo, TableStructure};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// File name of dbear's own known hosts, in the app's data folder.
const KNOWN_HOSTS_FILE: &str = "known_hosts";

fn failed(message: impl Into<String>) -> Error {
    Error::ConnectionFailed(message.into())
}

/// Where the SSH server reaches the database: `host:port` of the connection.
fn target(config: &ConnectionConfig) -> (String, u16) {
    (config.host.trim().to_string(), config.port.or(config.kind.default_port()).unwrap_or(0))
}

/// An open tunnel. Dropping it stops listening and closes the SSH session.
pub(crate) struct Tunnel {
    port: u16,
    session: Arc<Handle<Client>>,
    accept: JoinHandle<()>,
}

impl Tunnel {
    /// Signs in to `ssh` and forwards a free local port to `target_host:target_port` (as seen from
    /// the SSH server). Checks once that the SSH server can reach the target, so a wrong database
    /// host is reported as such rather than as a database connection error.
    pub(crate) async fn open(ssh: &SshTunnel, target_host: &str, target_port: u16) -> Result<Self> {
        let host = ssh.host.trim();
        let port = ssh.port();
        let rejected_key = Arc::new(Mutex::new(None));
        let handler = Client { host: host.to_string(), port, rejected_key: rejected_key.clone() };
        let config = Arc::new(client::Config {
            // Notice dead sessions (sleep, network change) so the next use opens a new tunnel.
            keepalive_interval: Some(Duration::from_secs(30)),
            keepalive_max: 3,
            inactivity_timeout: None,
            nodelay: true,
            ..Default::default()
        });
        let connecting = client::connect(config, (host, port), handler);
        let mut session = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
            Err(_) => return Err(failed(format!("SSH: {host}:{port} didn’t answer in {}s.", CONNECT_TIMEOUT.as_secs()))),
            Ok(Err(e)) => {
                if let Some(reason) = rejected_key.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    return Err(failed(reason));
                }
                return Err(failed(format!("SSH: couldn’t connect to {host}:{port}: {}", describe(&e))));
            }
            Ok(Ok(session)) => session,
        };
        authenticate(&mut session, ssh).await?;
        let session = Arc::new(session);

        // Fail now, with a clear message, if the SSH server can't reach the database.
        match session.channel_open_direct_tcpip(target_host, target_port.into(), "127.0.0.1", 0).await {
            Ok(channel) => {
                let _ = channel.close().await;
            }
            Err(e) => {
                return Err(failed(format!(
                    "SSH: the server at {host} couldn’t open a connection to {target_host}:{target_port} ({}). Check the database host and port as seen from the SSH server, and that it allows port forwarding.",
                    describe(&e)
                )));
            }
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| failed(format!("SSH: couldn’t listen on a local port: {e}")))?;
        let local_port = listener.local_addr().map_err(|e| failed(e.to_string()))?.port();
        let (forward, target_host) = (session.clone(), target_host.to_string());
        let accept = tokio::spawn(async move {
            while let Ok((mut socket, peer)) = listener.accept().await {
                let (session, target_host) = (forward.clone(), target_host.clone());
                tokio::spawn(async move {
                    let opened = session
                        .channel_open_direct_tcpip(target_host, target_port.into(), peer.ip().to_string(), peer.port().into())
                        .await;
                    // On failure the socket closes and the driver reports the connection error.
                    if let Ok(channel) = opened {
                        let mut stream = channel.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                    }
                });
            }
        });
        Ok(Self { port: local_port, session, accept })
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// The SSH session is still up (it ends when the server or the network drops it).
    pub(crate) fn is_alive(&self) -> bool {
        !self.session.is_closed() && !self.accept.is_finished()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
        let session = self.session.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = session.disconnect(russh::Disconnect::ByApplication, "", "en").await;
            });
        }
    }
}

/// Opens a tunnel for `config` when it has one: returns it with the config to connect with
/// (pointed at the tunnel). Without one, returns the config unchanged. Dumps and restores, which
/// connect on their own, keep the tunnel for as long as they run.
pub(crate) async fn route(config: ConnectionConfig) -> Result<(Option<Tunnel>, ConnectionConfig)> {
    let Some(ssh) = config.ssh.as_ref().filter(|s| s.forwarded_port.is_none()) else { return Ok((None, config)) };
    if !config.supports_ssh() {
        return Err(Error::InvalidConfig(format!("{} connections can’t use an SSH tunnel.", config.kind.display_name())));
    }
    let (host, port) = target(&config);
    let tunnel = Tunnel::open(ssh, &host, port).await?;
    let mut routed = config;
    if let Some(ssh) = routed.ssh.as_mut() {
        ssh.forwarded_port = Some(tunnel.port());
    }
    Ok((Some(tunnel), routed))
}

/// "connection refused" rather than russh's `IO(Os { … })`.
fn describe(e: &russh::Error) -> String {
    match e {
        russh::Error::IO(io) => io.to_string(),
        russh::Error::ChannelOpenFailure(reason) => format!("{reason:?}").to_lowercase().replace('_', " "),
        other => other.to_string(),
    }
}

async fn authenticate(session: &mut Handle<Client>, ssh: &SshTunnel) -> Result<()> {
    let user = ssh.user.trim();
    let host = ssh.host.trim();
    if user.is_empty() {
        return Err(Error::InvalidConfig("Enter the SSH user.".into()));
    }
    let sign_in_error = |e: russh::Error| failed(format!("SSH: signing in to {host} failed: {}", describe(&e)));
    let accepted = match ssh.auth {
        SshAuth::Password => {
            let password = ssh.secret.clone().unwrap_or_default();
            session.authenticate_password(user, password).await.map_err(sign_in_error)?.success()
        }
        SshAuth::PrivateKey => {
            let path = crate::paths::expand_home(ssh.key_path.trim());
            let passphrase = ssh.secret.as_deref().filter(|p| !p.is_empty());
            let key = keys::load_secret_key(&path, passphrase).map_err(|e| key_error(&path, passphrase.is_some(), &e))?;
            let hash = session.best_supported_rsa_hash().await.map_err(sign_in_error)?.flatten();
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
            session.authenticate_publickey(user, key).await.map_err(sign_in_error)?.success()
        }
        SshAuth::Agent => authenticate_with_agent(session, user).await?,
    };
    if accepted {
        return Ok(());
    }
    let what = match ssh.auth {
        SshAuth::Password => "password",
        SshAuth::PrivateKey => "key",
        SshAuth::Agent => "SSH agent’s keys",
    };
    Err(failed(format!("SSH: {host} didn’t accept the {what} for user “{user}”.")))
}

fn key_error(path: &std::path::Path, has_passphrase: bool, e: &keys::Error) -> Error {
    let shown = path.display();
    match e {
        keys::Error::IO(io) => failed(format!("SSH: couldn’t read the key {shown}: {io}")),
        keys::Error::KeyIsEncrypted if !has_passphrase => failed(format!("SSH: the key {shown} needs its passphrase.")),
        keys::Error::KeyIsEncrypted => failed(format!("SSH: wrong passphrase for the key {shown}.")),
        // Decrypting with the wrong passphrase yields garbage the key check rejects.
        keys::Error::SshKey(keys::ssh_key::Error::Crypto) if has_passphrase => {
            failed(format!("SSH: wrong passphrase for the key {shown}."))
        }
        other => failed(format!("SSH: couldn’t use the key {shown}: {other}")),
    }
}

#[cfg(unix)]
async fn agent() -> Result<keys::agent::client::AgentClient<Box<dyn keys::agent::client::AgentStream + Send + Unpin>>> {
    keys::agent::client::AgentClient::connect_env()
        .await
        .map(|a| a.dynamic())
        .map_err(|e| failed(format!("SSH: no SSH agent is running ({e}).")))
}

#[cfg(windows)]
async fn agent() -> Result<keys::agent::client::AgentClient<Box<dyn keys::agent::client::AgentStream + Send + Unpin>>> {
    // Windows' own OpenSSH agent, else PuTTY's Pageant.
    if let Ok(agent) = keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        return Ok(agent.dynamic());
    }
    keys::agent::client::AgentClient::connect_pageant()
        .await
        .map(|a| a.dynamic())
        .map_err(|e| failed(format!("SSH: no SSH agent is running ({e}).")))
}

/// Tries each of the agent's keys in turn.
async fn authenticate_with_agent(session: &mut Handle<Client>, user: &str) -> Result<bool> {
    let mut agent = agent().await?;
    let identities = agent.request_identities().await.map_err(|e| failed(format!("SSH: couldn’t list the agent’s keys: {e}")))?;
    if identities.is_empty() {
        return Err(failed("SSH: the SSH agent has no keys (add one with `ssh-add`)."));
    }
    let hash = session.best_supported_rsa_hash().await.map_err(|e| failed(format!("SSH: {}", describe(&e))))?.flatten();
    for identity in identities {
        let key = identity.public_key().into_owned();
        match session.authenticate_publickey_with(user, key, hash, &mut agent).await {
            Ok(result) if result.success() => return Ok(true),
            Ok(_) => continue,
            Err(e) => return Err(failed(format!("SSH: the agent couldn’t sign in: {e}"))),
        }
    }
    Ok(false)
}

/// Checks the SSH server's host key (see the module docs).
struct Client {
    host: String,
    port: u16,
    /// Why the key was refused, for the error the connection fails with.
    rejected_key: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
        };
        match check_host_key(&self.host, self.port, &key, user_known_hosts(), dbear_known_hosts()) {
            Ok(()) => Ok(true),
            Err(reason) => {
                *self.rejected_key.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
                Ok(false)
            }
        }
    }
}

fn user_known_hosts() -> Option<PathBuf> {
    crate::paths::home_dir().map(|home| home.join(".ssh").join("known_hosts"))
}

/// dbear's known hosts, in its data folder (`DBEAR_KNOWN_HOSTS` points elsewhere, for tests).
fn dbear_known_hosts() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DBEAR_KNOWN_HOSTS").filter(|p| !p.is_empty()) {
        return Some(path.into());
    }
    crate::paths::app_data_dir().map(|dir| dir.join("dbear").join(KNOWN_HOSTS_FILE))
}

/// Accepts a key listed for the host in the user's or dbear's known hosts. Refuses one that
/// differs from what either lists. Trusts (and records in dbear's file) a host neither lists.
fn check_host_key(host: &str, port: u16, key: &PublicKey, user: Option<PathBuf>, ours: Option<PathBuf>) -> std::result::Result<(), String> {
    let fingerprint = key.fingerprint(keys::HashAlg::Sha256);
    let changed = |file: &std::path::Path, line: usize| {
        format!(
            "SSH: the host key of {host} changed ({fingerprint}) and doesn’t match line {line} of {}. If the server was reinstalled, remove that line; otherwise someone may be intercepting the connection.",
            file.display()
        )
    };
    for file in [&user, &ours].into_iter().flatten() {
        match keys::check_known_hosts_path(host, port, key, file) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(keys::Error::KeyChanged { line }) => return Err(changed(file, line)),
            // An unreadable or malformed file: as if it didn't list the host.
            Err(_) => {}
        }
    }
    let Some(ours) = ours else { return Ok(()) };
    if let Some(dir) = ours.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Not remembering it only means it's trusted the same way next time.
    let _ = keys::known_hosts::learn_known_hosts_path(host, port, key, &ours);
    Ok(())
}

/// A driver whose server is reached through an SSH tunnel: opens the tunnel on first use (and
/// again after the SSH session drops), and hands every call to the real driver behind it.
pub(crate) struct TunneledDriver {
    config: ConnectionConfig,
    make: fn(ConnectionConfig) -> Arc<dyn Driver>,
    open: tokio::sync::Mutex<Option<(Tunnel, Arc<dyn Driver>)>>,
}

impl TunneledDriver {
    pub(crate) fn new(config: ConnectionConfig, make: fn(ConnectionConfig) -> Arc<dyn Driver>) -> Self {
        Self { config, make, open: tokio::sync::Mutex::new(None) }
    }

    async fn driver(&self) -> Result<Arc<dyn Driver>> {
        let mut open = self.open.lock().await;
        if let Some((tunnel, driver)) = open.as_ref() {
            if tunnel.is_alive() {
                return Ok(driver.clone());
            }
        }
        if let Some((_, old)) = open.take() {
            old.disconnect().await;
        }
        let (tunnel, routed) = route(self.config.clone()).await?;
        let tunnel = tunnel.ok_or_else(|| Error::Internal("no SSH tunnel configured".into()))?;
        let driver = (self.make)(routed);
        *open = Some((tunnel, driver.clone()));
        Ok(driver)
    }

    /// The driver behind an open tunnel, without opening one.
    async fn current(&self) -> Option<Arc<dyn Driver>> {
        self.open.lock().await.as_ref().filter(|(t, _)| t.is_alive()).map(|(_, d)| d.clone())
    }
}

#[async_trait]
impl Driver for TunneledDriver {
    fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    async fn connect(&self) -> Result<()> {
        self.driver().await?.connect().await
    }

    async fn disconnect(&self) {
        let open = self.open.lock().await.take();
        if let Some((tunnel, driver)) = open {
            driver.disconnect().await;
            drop(tunnel);
        }
    }

    async fn is_connected(&self) -> bool {
        match self.current().await {
            Some(driver) => driver.is_connected().await,
            None => false,
        }
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        self.driver().await?.list_databases().await
    }

    async fn list_schemas(&self) -> Result<Vec<Schema>> {
        self.driver().await?.list_schemas().await
    }

    async fn list_columns(&self) -> Result<Vec<TableColumns>> {
        self.driver().await?.list_columns().await
    }

    async fn fetch_rows(&self, table: &TableInfo, query: &RowQuery, limit: u32, offset: u64) -> Result<QueryResult> {
        self.driver().await?.fetch_rows(table, query, limit, offset).await
    }

    async fn fetch_page(&self, table: &TableInfo, query: &RowQuery, limit: u32, after: Option<&PageCursor>) -> Result<RowPage> {
        self.driver().await?.fetch_page(table, query, limit, after).await
    }

    async fn describe_table(&self, table: &TableInfo) -> Result<TableStructure> {
        self.driver().await?.describe_table(table).await
    }

    async fn execute(&self, sql: &str, max_rows: Option<u32>) -> Result<QueryResult> {
        self.driver().await?.execute(sql, max_rows).await
    }

    async fn cancel(&self) {
        if let Some(driver) = self.current().await {
            driver.cancel().await;
        }
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<u64> {
        self.driver().await?.apply(statements).await
    }

    async fn list_roles(&self) -> Result<Vec<Role>> {
        self.driver().await?.list_roles().await
    }

    async fn list_grants(&self, role: &RoleRef) -> Result<Vec<Grant>> {
        self.driver().await?.list_grants(role).await
    }

    async fn database_level(&self, role: &RoleRef, database: &str) -> Result<DatabaseLevelContext> {
        self.driver().await?.database_level(role, database).await
    }

    async fn list_database_access(&self, role: &RoleRef) -> Result<Vec<DatabaseAccess>> {
        self.driver().await?.list_database_access(role).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> PublicKey {
        let private = keys::PrivateKey::from(keys::ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]));
        private.public_key().clone()
    }

    #[test]
    fn trusts_new_hosts_once_and_refuses_changed_keys() {
        let dir = tempfile::tempdir().unwrap();
        let (user, ours) = (dir.path().join("user_known_hosts"), dir.path().join("dbear/known_hosts"));
        let check = |host: &str, port: u16, k: &PublicKey| check_host_key(host, port, k, Some(user.clone()), Some(ours.clone()));

        // First sight: trusted and remembered in dbear's file (the user's isn't created).
        assert_eq!(check("db.example.com", 22, &key(1)), Ok(()));
        assert!(ours.exists() && !user.exists());
        assert_eq!(check("db.example.com", 22, &key(1)), Ok(()));
        // Another port is another host.
        assert_eq!(check("db.example.com", 2222, &key(2)), Ok(()));

        let err = check("db.example.com", 22, &key(3)).unwrap_err();
        assert!(err.contains("changed") && err.contains("known_hosts"), "{err}");
    }

    #[test]
    fn follows_the_users_known_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let (user, ours) = (dir.path().join("known_hosts"), dir.path().join("dbear_known_hosts"));
        keys::known_hosts::learn_known_hosts_path("bastion", 22, &key(1), &user).unwrap();
        assert_eq!(check_host_key("bastion", 22, &key(1), Some(user.clone()), Some(ours.clone())), Ok(()));
        // Listed by the user: not copied to dbear's file.
        assert!(!ours.exists());
        let err = check_host_key("bastion", 22, &key(2), Some(user.clone()), Some(ours.clone())).unwrap_err();
        assert!(err.contains(&user.display().to_string()), "{err}");
    }

    #[test]
    fn route_leaves_connections_without_a_tunnel_alone() {
        let config = ConnectionConfig::new_empty(crate::DatabaseKind::Postgres);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (tunnel, routed) = rt.block_on(route(config.clone())).unwrap();
        assert!(tunnel.is_none());
        assert_eq!(routed, config);
    }
}
