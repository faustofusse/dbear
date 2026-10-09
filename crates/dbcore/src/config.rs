//! Editing helpers for [`ConnectionConfig`]: validation and connection URLs.
//! They live in the core so every frontend's "Add Connection" form behaves the same.

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use url::Url;

use crate::driver::{Error, Result};
use crate::model::{ConnectionConfig, DatabaseKind, SslMode};

/// Characters escaped in the user/password part of a URL.
const USERINFO: &AsciiSet = &CONTROLS
    .add(b' ').add(b'"').add(b'#').add(b'%').add(b'/').add(b':').add(b';').add(b'<').add(b'=').add(b'>')
    .add(b'?').add(b'@').add(b'[').add(b'\\').add(b']').add(b'^').add(b'`').add(b'{').add(b'|').add(b'}');

impl ConnectionConfig {
    /// An empty config for the "Add Connection" form.
    pub fn new_empty(kind: DatabaseKind) -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            group: String::new(),
            kind,
            host: if kind.is_sqlite_family() { String::new() } else { "localhost".into() },
            port: None,
            database: String::new(),
            user: None,
            password: None,
            // Turso is always reached over the internet: verify its certificate by default.
            ssl_mode: if kind == DatabaseKind::Libsql { SslMode::VerifyFull } else { SslMode::default() },
            show_all_databases: !kind.is_sqlite_family(),
            ssh: None,
        }
    }

    /// Checks what the form needs before saving or testing. Returns the first problem found.
    pub fn validate(&self) -> Result<()> {
        // Name and database are optional (see `default_name` / `default_database`).
        let invalid = |msg: &str| Err(Error::InvalidConfig(msg.into()));
        if self.ssh.is_some() && !self.supports_ssh() {
            return Err(Error::InvalidConfig(format!("{} connections can’t use an SSH tunnel.", self.kind.display_name())));
        }
        if self.kind == DatabaseKind::Sqlite {
            if self.database.trim().is_empty() {
                return invalid("Choose a database file.");
            }
            return Ok(());
        }
        if self.kind == DatabaseKind::Libsql {
            return crate::libsql::url::validate(self);
        }
        if self.host.trim().is_empty() {
            return invalid("Enter a host.");
        }
        if self.port == Some(0) {
            return invalid("Port must be between 1 and 65535.");
        }
        if let Some(ssh) = &self.ssh {
            if ssh.host.trim().is_empty() {
                return invalid("Enter the SSH host.");
            }
            if ssh.user.trim().is_empty() {
                return invalid("Enter the SSH user.");
            }
            if ssh.port == Some(0) {
                return invalid("SSH port must be between 1 and 65535.");
            }
            if ssh.auth == crate::model::SshAuth::PrivateKey && ssh.key_path.trim().is_empty() {
                return invalid("Choose the SSH private key file.");
            }
        }
        Ok(())
    }

    /// Parses `postgres://user:pass@host:5432/db?sslmode=require`, `mysql://…`, `sqlserver://…` (or `mssql://`),
    /// `sqlite:///path/file.db` or a Turso / libSQL URL (`libsql://db-org.turso.io?authToken=…`, `https://…`,
    /// `http://localhost:8080`). SQL Server URLs also take `encrypt` and `trustServerCertificate`.
    /// The name defaults to [`ConnectionConfig::default_name`]; id and group are left empty.
    pub fn from_url(input: &str) -> Result<Self> {
        let invalid = |msg: String| Error::InvalidConfig(msg);
        let url = Url::parse(input.trim()).map_err(|e| invalid(format!("Not a valid connection URL ({e}).")))?;
        let kind = match url.scheme() {
            "postgres" | "postgresql" => DatabaseKind::Postgres,
            "mysql" | "mariadb" => DatabaseKind::Mysql,
            "sqlite" | "file" => DatabaseKind::Sqlite,
            "libsql" | "http" | "https" | "ws" | "wss" => return crate::libsql::url::from_url(&url),
            "sqlserver" | "mssql" => DatabaseKind::SqlServer,
            other => return Err(invalid(format!("Unsupported URL scheme “{other}”."))),
        };
        let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
        let mut config = Self::new_empty(kind);

        if kind == DatabaseKind::Sqlite {
            // sqlite:///abs/path or sqlite://relative/path
            let path = format!("{}{}", url.host_str().unwrap_or(""), url.path());
            config.database = decode(&path);
        } else {
            config.host = url.host_str().unwrap_or("localhost").trim_start_matches('[').trim_end_matches(']').into();
            config.port = url.port();
            config.database = decode(url.path().trim_start_matches('/'));
            config.user = Some(decode(url.username())).filter(|u| !u.is_empty());
            config.password = url.password().map(decode);
            let mut encrypt: Option<String> = None;
            let mut trust_cert: Option<bool> = None;
            for (key, value) in url.query_pairs() {
                if kind == DatabaseKind::SqlServer && key.eq_ignore_ascii_case("encrypt") {
                    encrypt = Some(value.to_ascii_lowercase());
                } else if kind == DatabaseKind::SqlServer && key.eq_ignore_ascii_case("trustServerCertificate") {
                    trust_cert = Some(matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1"));
                } else if key == "sslmode" || key == "ssl-mode" {
                    config.ssl_mode = match value.to_ascii_lowercase().replace('_', "-").as_str() {
                        "disable" | "disabled" => SslMode::Disable,
                        "allow" | "prefer" | "preferred" => SslMode::Prefer,
                        "require" | "required" => SslMode::Require,
                        "verify-ca" | "verify-full" | "verify-identity" => SslMode::VerifyFull,
                        other => return Err(invalid(format!("Unknown sslmode “{other}”."))),
                    };
                } else if kind == DatabaseKind::SqlServer && key.eq_ignore_ascii_case("instance") && !value.is_empty() {
                    config.host = format!("{}\\{value}", config.host);
                }
            }
            if encrypt.is_some() || trust_cert.is_some() {
                config.ssl_mode = sql_server_ssl_mode(encrypt.as_deref(), trust_cert).map_err(invalid)?;
            }
        }
        config.name = config.default_name();
        Ok(config)
    }

    /// The connection as a URL, e.g. for "Copy URL". The password is only included when asked for.
    pub fn to_url(&self, include_password: bool) -> String {
        if self.kind == DatabaseKind::Sqlite {
            return format!("sqlite://{}", self.database);
        }
        if self.kind == DatabaseKind::Libsql {
            return crate::libsql::url::to_url(self, include_password);
        }
        let scheme = match self.kind {
            DatabaseKind::Postgres => "postgres",
            DatabaseKind::SqlServer => "sqlserver",
            _ => "mysql",
        };
        let enc = |s: &str| utf8_percent_encode(s, USERINFO).to_string();
        let mut userinfo = String::new();
        if let Some(user) = self.user.as_deref().filter(|u| !u.is_empty()) {
            userinfo.push_str(&enc(user));
            if let (true, Some(pw)) = (include_password, self.password.as_deref()) {
                userinfo.push(':');
                userinfo.push_str(&enc(pw));
            }
            userinfo.push('@');
        }
        // SQL Server named instances (`host\\SQLEXPRESS`) go in a query parameter.
        let (host, instance) = match self.host.split_once('\\') {
            Some((host, instance)) if self.kind == DatabaseKind::SqlServer => (host, Some(instance)),
            _ => (self.host.as_str(), None),
        };
        let host = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        let mut params = Vec::new();
        match self.ssl_mode {
            SslMode::Prefer => {}
            SslMode::Disable => params.push("sslmode=disable".to_string()),
            SslMode::Require => params.push("sslmode=require".to_string()),
            SslMode::VerifyFull => params.push("sslmode=verify-full".to_string()),
        }
        if let Some(instance) = instance {
            params.push(format!("instance={}", utf8_percent_encode(instance, USERINFO)));
        }
        let query = if params.is_empty() { String::new() } else { format!("?{}", params.join("&")) };
        format!("{scheme}://{userinfo}{host}{port}/{}{query}", enc(&self.database))
    }
}

/// SQL Server's `encrypt` / `trustServerCertificate` (JDBC, ADO.NET, DBeaver) as an [`SslMode`]:
/// - `encrypt=false|optional`: only the login is encrypted → `Disable`.
/// - `encrypt=true|mandatory|strict` (the JDBC default) with `trustServerCertificate=true` → `Require`,
///   without it → `VerifyFull`.
pub fn sql_server_ssl_mode(encrypt: Option<&str>, trust_server_certificate: Option<bool>) -> Result<SslMode, String> {
    let encrypt = match encrypt.map(|e| e.trim().to_ascii_lowercase()) {
        None => true,
        Some(e) => match e.as_str() {
            "false" | "no" | "0" | "optional" | "disable" | "disabled" => false,
            "true" | "yes" | "1" | "mandatory" | "strict" | "require" | "required" => true,
            other => return Err(format!("Unknown encrypt setting “{other}”.")),
        },
    };
    Ok(match (encrypt, trust_server_certificate) {
        (false, _) => SslMode::Disable,
        (true, Some(true)) => SslMode::Require,
        (true, _) => SslMode::VerifyFull,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_postgres_url() {
        let c = ConnectionConfig::from_url("postgresql://app%40corp:p%3Ass@db.example.com:6543/billing?sslmode=verify-full")
            .unwrap();
        assert_eq!(c.kind, DatabaseKind::Postgres);
        assert_eq!(c.host, "db.example.com");
        assert_eq!(c.port, Some(6543));
        assert_eq!(c.database, "billing");
        assert_eq!(c.name, "billing");
        assert_eq!(c.user.as_deref(), Some("app@corp"));
        assert_eq!(c.password.as_deref(), Some("p:ss"));
        assert_eq!(c.ssl_mode, SslMode::VerifyFull);
    }

    #[test]
    fn parses_minimal_and_ipv6_urls() {
        let c = ConnectionConfig::from_url("postgres://[::1]/app").unwrap();
        assert_eq!((c.host.as_str(), c.port, c.user), ("::1", None, None));
        let s = ConnectionConfig::from_url("sqlite:///Users/me/notes.db").unwrap();
        assert_eq!((s.kind, s.database.as_str(), s.name.as_str()), (DatabaseKind::Sqlite, "/Users/me/notes.db", "notes.db"));
    }

    #[test]
    fn defaults_name_and_database() {
        let c = ConnectionConfig::from_url("postgres://u@db.example.com:5432").unwrap();
        assert_eq!((c.name.as_str(), c.database.as_str(), c.default_database()), ("db.example.com", "", "postgres"));
        assert_eq!(c.summary(), "PostgreSQL · db.example.com:5432");
        let tunneled = ConnectionConfig { ssh: Some(crate::model::SshTunnel { host: "bastion".into(), ..Default::default() }), ..c.clone() };
        assert_eq!(tunneled.summary(), "PostgreSQL · db.example.com:5432 via bastion");
        let c = ConnectionConfig::from_url("postgres://u@db.example.com/app").unwrap();
        assert_eq!((c.name.as_str(), c.default_database()), ("app", "app"));
        let m = ConnectionConfig::from_url("mysql://root@localhost").unwrap();
        assert_eq!((m.name.as_str(), m.default_database()), ("localhost", ""));
    }

    #[test]
    fn parses_sql_server_urls() {
        let c = ConnectionConfig::from_url("sqlserver://sa:p%40ss@db.example.com:14339/app?encrypt=true&trustServerCertificate=true").unwrap();
        assert_eq!((c.kind, c.host.as_str(), c.port, c.database.as_str()), (DatabaseKind::SqlServer, "db.example.com", Some(14339), "app"));
        assert_eq!((c.user.as_deref(), c.password.as_deref(), c.ssl_mode), (Some("sa"), Some("p@ss"), SslMode::Require));
        assert_eq!(c.to_url(true), "sqlserver://sa:p%40ss@db.example.com:14339/app?sslmode=require");

        let c = ConnectionConfig::from_url("mssql://u@h/?encrypt=false").unwrap();
        assert_eq!((c.ssl_mode, c.default_database(), c.name.as_str()), (SslMode::Disable, "master", "h"));
        assert_eq!(ConnectionConfig::from_url("sqlserver://h/db?encrypt=true").unwrap().ssl_mode, SslMode::VerifyFull);
        assert_eq!(ConnectionConfig::from_url("sqlserver://h/db?sslmode=require").unwrap().ssl_mode, SslMode::Require);
        assert!(ConnectionConfig::from_url("sqlserver://h/db?encrypt=maybe").is_err());

        let named = ConnectionConfig::from_url("sqlserver://h/db?instance=SQLEXPRESS").unwrap();
        assert_eq!(named.host, "h\\SQLEXPRESS");
        assert_eq!(named.to_url(false), "sqlserver://h/db?instance=SQLEXPRESS");
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(matches!(ConnectionConfig::from_url("redis://x"), Err(Error::InvalidConfig(_))));
        assert!(matches!(ConnectionConfig::from_url("not a url"), Err(Error::InvalidConfig(_))));
        assert!(matches!(ConnectionConfig::from_url("postgres://h/db?sslmode=nope"), Err(Error::InvalidConfig(_))));
    }

    #[test]
    fn url_round_trips() {
        let url = "postgres://app%40corp:p%3Ass@db.example.com:6543/billing?sslmode=require";
        let c = ConnectionConfig::from_url(url).unwrap();
        assert_eq!(c.to_url(true), url);
        assert_eq!(c.to_url(false), "postgres://app%40corp@db.example.com:6543/billing?sslmode=require");
    }

    #[test]
    fn validates_required_fields() {
        // Name and database are optional for servers.
        let mut c = ConnectionConfig::new_empty(DatabaseKind::Postgres);
        assert!(c.validate().is_ok());
        assert!(ConnectionConfig::new_empty(DatabaseKind::Sqlite).validate().is_err());
        c.port = Some(0);
        assert!(c.validate().is_err());
        c.port = None;
        c.host = " ".into();
        assert!(c.validate().is_err());
    }
}
