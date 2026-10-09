//! SSH tunnels against the dev SSH server, reaching the dev Postgres and MySQL at their address on
//! the containers' network. Skipped unless `DBEAR_TEST_SSH=1`; `scripts/test-ssh.sh` sets it up.

use std::path::PathBuf;
use std::sync::Arc;

use dbcore::dump::{self, CancelToken, DumpOptions, DumpProgress};
use dbcore::{Connection, ConnectionConfig, DatabaseKind, Error, SshAuth, SshTunnel, Value};

fn enabled() -> bool {
    let on = std::env::var("DBEAR_TEST_SSH").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: run scripts/test-ssh.sh");
    } else {
        static KNOWN_HOSTS: std::sync::Once = std::sync::Once::new();
        // Host keys go to a throwaway file, not dbear's real known_hosts.
        KNOWN_HOSTS.call_once(|| {
            // SAFETY: set once, before any tunnel opens.
            unsafe { std::env::set_var("DBEAR_KNOWN_HOSTS", known_hosts()) };
        });
    }
    on
}

fn known_hosts() -> PathBuf {
    std::env::temp_dir().join(format!("dbear-test-known-hosts-{}", std::process::id()))
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

fn key(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/ssh").join(name).display().to_string()
}

fn tunnel(auth: SshAuth, secret: Option<&str>) -> SshTunnel {
    SshTunnel {
        host: "127.0.0.1".into(),
        port: Some(22229),
        user: "dbear".into(),
        auth,
        key_path: if auth == SshAuth::PrivateKey { key("id_ed25519") } else { String::new() },
        secret: secret.map(Into::into),
        forwarded_port: None,
    }
}

/// The dev Postgres, as the SSH server sees it.
fn postgres(ssh: SshTunnel) -> ConnectionConfig {
    ConnectionConfig {
        id: "ssh-pg".into(),
        name: "app_dev over ssh".into(),
        host: std::env::var("DBEAR_TEST_SSH_POSTGRES_HOST").unwrap(),
        port: Some(5432),
        database: "app_dev".into(),
        user: Some("postgres".into()),
        password: Some("postgres".into()),
        ssh: Some(ssh),
        ..ConnectionConfig::new_empty(DatabaseKind::Postgres)
    }
}

fn mysql(ssh: SshTunnel) -> ConnectionConfig {
    ConnectionConfig {
        id: "ssh-my".into(),
        name: "mysql over ssh".into(),
        host: std::env::var("DBEAR_TEST_SSH_MYSQL_HOST").unwrap(),
        port: Some(3306),
        database: "shop".into(),
        user: Some("root".into()),
        password: Some("mysql".into()),
        ssh: Some(ssh),
        ..ConnectionConfig::new_empty(DatabaseKind::Mysql)
    }
}

fn error_text(config: ConnectionConfig) -> String {
    match block_on(Connection::new(config).list_schemas()) {
        Ok(_) => panic!("expected the connection to fail"),
        Err(Error::ConnectionFailed(m)) => m,
        Err(e) => panic!("{e:?}"),
    }
}

#[test]
fn browses_postgres_through_a_password_tunnel() {
    if !enabled() {
        return;
    }
    let conn = Connection::new(postgres(tunnel(SshAuth::Password, Some("dbear"))));
    let schemas = block_on(conn.list_schemas()).unwrap();
    assert!(schemas.iter().any(|s| s.name == "public"), "{schemas:?}");
    let result = block_on(conn.execute("select count(*) from users".into())).unwrap();
    assert!(matches!(result.rows[0][0], Value::Int(n) if n > 0), "{result:?}");
    assert!(block_on(conn.is_connected()));
    // The config the app sees is still the one it saved (no local port).
    assert_eq!(conn.config().ssh.as_ref().and_then(|s| s.forwarded_port), None);

    // Disconnecting closes the tunnel; the next query opens a new one.
    block_on(conn.disconnect());
    assert!(!block_on(conn.is_connected()));
    let again = block_on(conn.execute("select 1".into())).unwrap();
    assert_eq!(again.rows[0][0], Value::Int(1));
}

#[test]
fn browses_mysql_through_a_key_tunnel() {
    if !enabled() {
        return;
    }
    let conn = Connection::new(mysql(tunnel(SshAuth::PrivateKey, None)));
    let result = block_on(conn.execute("select database()".into())).unwrap();
    assert_eq!(result.rows[0][0], Value::Text("shop".into()));
    // Other databases open their own tunnel.
    let databases = block_on(conn.list_databases()).unwrap();
    assert!(databases.contains(&"blog".to_string()), "{databases:?}");
}

#[test]
fn signs_in_with_the_ssh_agent() {
    if !enabled() {
        return;
    }
    // scripts/test-ssh.sh runs a private agent holding the dev key.
    let conn = Connection::new(postgres(tunnel(SshAuth::Agent, None)));
    assert_eq!(block_on(conn.execute("select 1".into())).unwrap().rows[0][0], Value::Int(1));
}

#[test]
fn uses_key_passphrases() {
    if !enabled() {
        return;
    }
    let with = |secret: Option<&str>| SshTunnel { key_path: key("id_ed25519_passphrase"), ..tunnel(SshAuth::PrivateKey, secret) };
    let conn = Connection::new(postgres(with(Some("dbear-passphrase"))));
    assert_eq!(block_on(conn.execute("select 1".into())).unwrap().rows[0][0], Value::Int(1));

    let missing = error_text(postgres(with(None)));
    assert!(missing.contains("needs its passphrase"), "{missing}");
    let wrong = error_text(postgres(with(Some("nope"))));
    assert!(wrong.contains("wrong passphrase"), "{wrong}");
}

#[test]
fn reports_ssh_failures_clearly() {
    if !enabled() {
        return;
    }
    let wrong_password = error_text(postgres(tunnel(SshAuth::Password, Some("wrong"))));
    assert!(wrong_password.contains("didn’t accept the password") && wrong_password.contains("dbear"), "{wrong_password}");

    let no_server = error_text(postgres(SshTunnel { port: Some(1), ..tunnel(SshAuth::Password, Some("dbear")) }));
    assert!(no_server.contains("SSH: couldn’t connect to 127.0.0.1:1"), "{no_server}");

    let no_key = error_text(postgres(SshTunnel { key_path: "/nonexistent/key".into(), ..tunnel(SshAuth::PrivateKey, None) }));
    assert!(no_key.contains("couldn’t read the key /nonexistent/key"), "{no_key}");

    // The SSH server signs in fine but can't reach the database.
    let closed = ConnectionConfig { port: Some(1), ..postgres(tunnel(SshAuth::Password, Some("dbear"))) };
    let unreachable = error_text(closed);
    assert!(unreachable.contains("couldn’t open a connection to") && unreachable.contains(":1"), "{unreachable}");

    // A host whose key changed is refused. The server is reached as `localhost` here, a name the
    // other tests don't use, with another key on record for it.
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(known_hosts()).unwrap();
    writeln!(file, "[localhost]:22229 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOmLx8yy8lzGRQmQ7mwsDw0Y9xbbsL0nbS4CE5oLz9hC").unwrap();
    let changed = error_text(postgres(SshTunnel { host: "localhost".into(), ..tunnel(SshAuth::Password, Some("dbear")) }));
    assert!(changed.contains("host key of localhost changed"), "{changed}");
}

#[test]
fn dumps_through_a_tunnel() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("shop.sql");
    let config = mysql(tunnel(SshAuth::Password, Some("dbear")));
    let progress: dump::ProgressFn<DumpProgress> = Arc::new(|_| {});
    let summary = block_on(dump::dump(config, out.clone(), DumpOptions::default(), progress, CancelToken::new())).unwrap();
    assert!(summary.tables > 0 && summary.rows > 0, "{summary:?}");
    assert!(std::fs::read_to_string(&out).unwrap().contains("CREATE TABLE"));
}
