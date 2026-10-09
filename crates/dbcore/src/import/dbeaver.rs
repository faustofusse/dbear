//! Reads DBeaver's saved connections (`data-sources*.json`) and their credentials.
//!
//! Layout: `<DBeaverData>/workspace6/<project>/.dbeaver/data-sources*.json`, with users and
//! passwords in `credentials-config.json` next to it, AES-128-CBC encrypted with a fixed key
//! that is public in DBeaver's source (it obfuscates, it doesn't protect).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use serde_json::Value as Json;

use super::{ImportScan, ImportedConnection, SkippedConnection};
use crate::config::sql_server_ssl_mode;
use crate::driver::{Error, Result};
use crate::model::{ConnectionConfig, DatabaseKind, SshAuth, SshTunnel, SslMode};

/// DBeaver's built-in key for `credentials-config.json`.
const CREDENTIALS_KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];

/// Where DBeaver keeps its data on this machine (existing directories only).
pub fn default_data_dirs() -> Vec<PathBuf> {
    use crate::paths;
    let Some(home) = paths::home_dir() else { return Vec::new() };
    let candidates = if cfg!(target_os = "macos") {
        vec![home.join("Library/DBeaverData")]
    } else if cfg!(windows) {
        paths::app_data_dir().map(|d| d.join("DBeaverData")).into_iter().collect()
    } else {
        vec![
            paths::xdg_data_dir().unwrap_or_else(|| home.join(".local/share")).join("DBeaverData"),
            home.join(".var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData"), // Flatpak
            home.join("snap/dbeaver-ce/current/.local/share/DBeaverData"),      // Snap
        ]
    };
    candidates.into_iter().filter(|p| p.is_dir()).collect()
}

/// Scans DBeaver's default locations.
pub fn scan_default() -> Result<ImportScan> {
    let dirs = default_data_dirs();
    if dirs.is_empty() {
        return Err(Error::InvalidConfig("DBeaver’s data folder wasn’t found. Choose its data-sources.json instead.".into()));
    }
    let mut scan = ImportScan::default();
    for dir in dirs {
        scan.merge(scan_path(&dir)?);
    }
    Ok(scan)
}

/// Scans a `data-sources*.json` file, or any folder containing them (a `.dbeaver` folder,
/// a project, a workspace or the whole DBeaverData folder).
pub fn scan_path(path: &Path) -> Result<ImportScan> {
    let files = if path.is_file() { vec![path.to_path_buf()] } else { find_data_sources(path, 4) };
    if files.is_empty() {
        return Err(Error::InvalidConfig(format!("No DBeaver connections found in {}.", path.display())));
    }
    let mut scan = ImportScan::default();
    for file in files {
        scan.merge(scan_file(&file)?);
    }
    Ok(scan)
}

/// `data-sources*.json` files under `dir`, sorted (so `data-sources.json` comes first).
fn find_data_sources(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files = Vec::new();
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if path.is_file() && name.starts_with("data-sources") && name.ends_with(".json") {
            files.push(path);
        } else if path.is_dir() && depth > 0 && (!name.starts_with('.') || name == ".dbeaver") {
            files.extend(find_data_sources(&path, depth.saturating_sub(1)));
        }
    }
    files
}

fn scan_file(file: &Path) -> Result<ImportScan> {
    let read_error = |e: &dyn std::fmt::Display| Error::InvalidConfig(format!("Couldn’t read {}: {e}", file.display()));
    let text = std::fs::read_to_string(file).map_err(|e| read_error(&e))?;
    let json: Json = serde_json::from_str(&text).map_err(|e| read_error(&e))?;
    let credentials = file.parent().map(|dir| read_credentials(&dir.join("credentials-config.json"))).unwrap_or_default();
    let project = project_name(file);
    Ok(parse_data_sources(&json, &credentials, project.as_deref()))
}

/// The DBeaver project, when it isn't the default one ("General").
fn project_name(file: &Path) -> Option<String> {
    let project = file.parent()?.parent()?.file_name()?.to_str()?;
    (project != "General" && !project.starts_with("workspace")).then(|| project.to_string())
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Credentials {
    pub user: Option<String>,
    pub password: Option<String>,
    /// The SSH tunnel's user and password (or key passphrase), under `network/ssh_tunnel`.
    pub ssh_user: Option<String>,
    pub ssh_password: Option<String>,
}

/// Users and passwords by connection id. A missing or unreadable file means none were saved.
fn read_credentials(path: &Path) -> HashMap<String, Credentials> {
    std::fs::read(path).ok().and_then(|bytes| decrypt_credentials(&bytes)).unwrap_or_default()
}

pub(crate) fn decrypt_credentials(bytes: &[u8]) -> Option<HashMap<String, Credentials>> {
    let (iv, ciphertext) = (bytes.get(..16)?, bytes.get(16..)?);
    let plain = cbc::Decryptor::<aes::Aes128>::new_from_slices(&CREDENTIALS_KEY, iv)
        .ok()?
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .ok()?;
    let json: Json = serde_json::from_slice(&plain).ok()?;
    Some(
        json.as_object()?
            .iter()
            .map(|(id, entry)| {
                let field = |section: &str, k: &str| entry[section][k].as_str().filter(|s| !s.is_empty()).map(String::from);
                let credentials = Credentials {
                    user: field("#connection", "user"),
                    password: field("#connection", "password"),
                    ssh_user: field("network/ssh_tunnel", "user"),
                    ssh_password: field("network/ssh_tunnel", "password"),
                };
                (id.clone(), credentials)
            })
            .collect(),
    )
}

pub(crate) fn parse_data_sources(json: &Json, credentials: &HashMap<String, Credentials>, project: Option<&str>) -> ImportScan {
    let mut scan = ImportScan::default();
    let Some(connections) = json["connections"].as_object() else { return scan };
    for (id, source) in connections {
        let name = source["name"].as_str().unwrap_or(id).to_string();
        match convert(id, source, credentials.get(id), project) {
            Ok(imported) => scan.connections.push(imported),
            Err(reason) => scan.skipped.push(SkippedConnection { name, reason }),
        }
    }
    // The JSON object's order isn't meaningful (ids); list by folder, then name.
    let key = |c: &ConnectionConfig| (c.group.to_lowercase(), c.name.to_lowercase());
    scan.connections.sort_by_key(|c| key(&c.config));
    scan.skipped.sort_by_key(|s| s.name.to_lowercase());
    scan
}

/// One DBeaver connection → ours, or why it can't be imported.
fn convert(id: &str, source: &Json, credentials: Option<&Credentials>, project: Option<&str>) -> Result<ImportedConnection, String> {
    let provider = source["provider"].as_str().unwrap_or_default();
    let driver = source["driver"].as_str().unwrap_or_default();
    let kind = match (provider, driver) {
        ("postgresql", _) => DatabaseKind::Postgres,
        ("mysql", _) => DatabaseKind::Mysql,
        (_, d) if d.contains("libsql") => return convert_libsql(id, source, credentials, project),
        ("sqlite", _) => DatabaseKind::Sqlite,
        ("generic", d) if d.contains("sqlite") => DatabaseKind::Sqlite,
        // `mssql` is the legacy provider id; Azure SQL and Babelfish connections use `sqlserver` too.
        ("sqlserver" | "mssql", _) => DatabaseKind::SqlServer,
        (other, _) => return Err(format!("{} isn’t supported yet.", provider_display_name(other))),
    };

    let conf = &source["configuration"];
    let text = |v: &Json| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let mut config = ConnectionConfig::new_empty(kind);
    config.name = text(&source["name"]).unwrap_or_default();
    config.group = [project.map(String::from), text(&source["folder"])].into_iter().flatten().collect::<Vec<_>>().join(" / ");

    // Fields first; the JDBC URL fills whatever they leave out ("URL" configurations).
    let from_url = text(&conf["url"]).and_then(|url| from_jdbc_url(kind, &url));
    if kind == DatabaseKind::Sqlite {
        config.database = text(&conf["database"]).or_else(|| from_url.as_ref().map(|c| c.database.clone())).unwrap_or_default();
        if config.database.is_empty() {
            return Err("No database file is set.".into());
        }
    } else {
        config.host = text(&conf["host"]).or_else(|| from_url.as_ref().map(|c| c.host.clone())).unwrap_or_else(|| "localhost".into());
        config.port = text(&conf["port"]).and_then(|p| p.parse().ok()).or(from_url.as_ref().and_then(|c| c.port));
        config.database = text(&conf["database"]).or_else(|| from_url.as_ref().map(|c| c.database.clone())).unwrap_or_default();
        config.user = credentials
            .and_then(|c| c.user.clone())
            .or_else(|| text(&conf["user"]))
            .or_else(|| from_url.as_ref().and_then(|c| c.user.clone()));
        if source["save-password"].as_bool() != Some(false) {
            config.password = credentials.and_then(|c| c.password.clone()).or_else(|| text(&conf["password"]));
        }
        config.ssl_mode = ssl_mode(kind, conf).or(from_url.as_ref().map(|c| c.ssl_mode)).unwrap_or_default();
    }

    let provider_flag = |key: &str| conf["provider-properties"][key].as_str().map(|v| v == "true");
    config.show_all_databases = match kind {
        // DBeaver shows only the configured Postgres database unless told otherwise…
        DatabaseKind::Postgres => provider_flag("@dbeaver-show-non-default-db@").unwrap_or(false),
        // …and every MySQL database by default.
        DatabaseKind::Mysql => provider_flag("@dbeaver-show-all-dbs@").unwrap_or(true),
        DatabaseKind::Sqlite | DatabaseKind::Libsql => false,
        // Every database by default, except for Azure SQL and Babelfish (DBeaver's SQLServerDataSource).
        DatabaseKind::SqlServer => {
            provider_flag("show-all-databases-azure").unwrap_or(!(driver.contains("azure") || driver.contains("babelfish")))
        }
    };
    if config.name.is_empty() {
        config.name = config.default_name();
    }

    let mut warnings = Vec::new();
    let tunnel = &conf["handlers"]["ssh_tunnel"];
    if tunnel["enabled"].as_bool() == Some(true) && kind != DatabaseKind::Sqlite {
        config.ssh = Some(ssh_tunnel(tunnel, credentials, &mut warnings));
    } else if conf["network-profile"].as_str().is_some_and(|p| !p.is_empty()) {
        warnings.push("Uses a shared network profile (SSH or proxy), which isn’t imported; set up the tunnel in dbear.".into());
    }
    let proxy = conf["handlers"].as_object().is_some_and(|h| h.iter().any(|(k, v)| k.contains("proxy") && v["enabled"].as_bool() == Some(true)));
    if proxy {
        warnings.push("Uses a proxy, which dbear doesn’t support yet.".into());
    }
    match conf["auth-model"].as_str() {
        None | Some("native") | Some("sqlserver_database") => {}
        Some(model) => warnings.push(format!("Uses “{model}” authentication; dbear will connect with a user and password.")),
    }
    if kind != DatabaseKind::Sqlite && config.password.is_none() {
        warnings.push("No saved password.".into());
    }
    Ok(ImportedConnection { config, source_id: id.to_string(), warnings, already_added: false })
}

/// DBeaver's `ssh_tunnel` handler: the SSH server in `properties`, its user and password (or key
/// passphrase) in the credentials file.
fn ssh_tunnel(handler: &Json, credentials: Option<&Credentials>, warnings: &mut Vec<String>) -> SshTunnel {
    let props = &handler["properties"];
    let text = |v: &Json| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let number = |v: &Json| v.as_u64().and_then(|n| u16::try_from(n).ok()).or_else(|| text(v).and_then(|s| s.parse().ok()));
    let auth = match text(&props["authType"]).as_deref() {
        Some("PUBLIC_KEY") => SshAuth::PrivateKey,
        Some("AGENT") => SshAuth::Agent,
        _ => SshAuth::Password,
    };
    let mut tunnel = SshTunnel {
        host: text(&props["host"]).unwrap_or_default(),
        port: number(&props["port"]).filter(|&p| p != SshTunnel::DEFAULT_PORT),
        user: credentials.and_then(|c| c.ssh_user.clone()).or_else(|| text(&props["user"])).unwrap_or_default(),
        auth,
        key_path: if auth == SshAuth::PrivateKey { text(&props["keyPath"]).unwrap_or_default() } else { String::new() },
        secret: None,
        forwarded_port: None,
    };
    if auth != SshAuth::Agent && handler["save-password"].as_bool() != Some(false) {
        tunnel.secret = credentials.and_then(|c| c.ssh_password.clone());
    }
    if props["jumpServers"].as_array().is_some_and(|j| !j.is_empty()) || text(&props["jumpServer"]).is_some() {
        warnings.push("Its SSH tunnel goes through jump servers, which dbear doesn’t support yet.".into());
    }
    if tunnel.host.is_empty() || tunnel.user.is_empty() {
        warnings.push("Its SSH tunnel has no host or user; fill them in before connecting.".into());
    } else if auth == SshAuth::Password && tunnel.secret.is_none() {
        warnings.push("No saved SSH password.".into());
    }
    tunnel
}

/// DBeaver's LibSQL driver (`libsql_jdbc`, Turso or sqld). The server URL is in the JDBC URL
/// (`jdbc:dbeaver:libsql:https://…`) or the `server` field; the auth token is the saved password
/// (its "LibSQL token" auth model has no user). A local file becomes a SQLite connection.
fn convert_libsql(id: &str, source: &Json, credentials: Option<&Credentials>, project: Option<&str>) -> Result<ImportedConnection, String> {
    let conf = &source["configuration"];
    let text = |v: &Json| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let server = text(&conf["url"])
        .map(|url| {
            let url = url.strip_prefix("jdbc:").unwrap_or(&url);
            let url = url.strip_prefix("dbeaver:").unwrap_or(url);
            url.strip_prefix("libsql:").unwrap_or(url).to_string()
        })
        .filter(|url| !url.is_empty())
        .or_else(|| text(&conf["server"]))
        .or_else(|| text(&conf["host"]))
        .or_else(|| text(&conf["database"]))
        .ok_or("No server URL is set.")?;
    let url = if server.contains("://") || server.starts_with("file:") {
        server
    } else if server.starts_with('/') || server.starts_with('~') {
        format!("sqlite://{server}")
    } else {
        format!("https://{server}")
    };
    let mut config = ConnectionConfig::from_url(&url).map_err(|e| e.to_string())?;
    if !matches!(config.kind, DatabaseKind::Libsql | DatabaseKind::Sqlite) {
        return Err(format!("“{url}” isn’t a libSQL URL."));
    }
    config.name = text(&source["name"]).unwrap_or_else(|| config.default_name());
    config.group = [project.map(String::from), text(&source["folder"])].into_iter().flatten().collect::<Vec<_>>().join(" / ");

    let mut warnings = Vec::new();
    if config.kind == DatabaseKind::Libsql {
        if source["save-password"].as_bool() != Some(false) {
            let saved = credentials.and_then(|c| c.password.clone()).or_else(|| text(&conf["password"]));
            config.password = saved.or(config.password);
        }
        if config.password.is_none() {
            warnings.push("No saved auth token.".into());
        }
    } else {
        config.password = None;
    }
    let handlers = conf["handlers"].as_object();
    if handlers.is_some_and(|h| h.iter().any(|(k, v)| (k == "ssh_tunnel" || k.contains("proxy")) && v["enabled"].as_bool() == Some(true))) {
        warnings.push("Uses an SSH tunnel or proxy, which dbear doesn’t support yet.".into());
    }
    Ok(ImportedConnection { config, source_id: id.to_string(), warnings, already_added: false })
}

/// `jdbc:postgresql://h:5432/db?sslmode=require`, `jdbc:mysql://…`, `jdbc:sqlite:/path/file.db`.
fn from_jdbc_url(kind: DatabaseKind, url: &str) -> Option<ConnectionConfig> {
    let url = url.strip_prefix("jdbc:").unwrap_or(url);
    if kind == DatabaseKind::SqlServer {
        return from_sql_server_jdbc_url(url);
    }
    if kind == DatabaseKind::Sqlite {
        let path = url.strip_prefix("sqlite:")?.trim_start_matches("file:");
        let path = path.split('?').next().unwrap_or(path);
        let mut config = ConnectionConfig::new_empty(kind);
        config.database = path.to_string();
        return Some(config);
    }
    // Only the SSL setting is kept from the driver parameters (`sslmode`, MySQL's `sslMode`/`useSSL`).
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let ssl = query.split('&').find_map(|param| {
        let (key, value) = param.split_once('=')?;
        match (key.to_ascii_lowercase().as_str(), value.to_ascii_lowercase().as_str()) {
            ("sslmode", mode) => Some(mode.to_string()),
            ("usessl", "false") => Some("disable".into()),
            _ => None,
        }
    });
    let url = match ssl {
        Some(mode) => format!("{base}?sslmode={mode}"),
        None => base.to_string(),
    };
    ConnectionConfig::from_url(&url).ok()
}

/// `sqlserver://host[\\instance][:port][;databaseName=db;encrypt=…;trustServerCertificate=…;user=…]`
/// (Microsoft's driver) or jTDS's `jtds:sqlserver://host[:port][/db][;instance=…]`.
fn from_sql_server_jdbc_url(url: &str) -> Option<ConnectionConfig> {
    let url = url.strip_prefix("jtds:").unwrap_or(url);
    let rest = url.strip_prefix("sqlserver://")?;
    let mut parts = rest.split(';');
    let address = parts.next().unwrap_or_default();
    let (address, path_db) = match address.split_once('/') {
        Some((a, db)) => (a, Some(db)),
        None => (address, None),
    };
    let (host, port) = match address.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse().ok()),
        _ => (address, None),
    };
    let mut config = ConnectionConfig::new_empty(DatabaseKind::SqlServer);
    config.host = if host.is_empty() { "localhost".into() } else { host.to_string() };
    config.port = port;
    config.database = path_db.unwrap_or_default().to_string();
    let (mut encrypt, mut trust) = (None, None);
    for param in parts {
        let Some((key, value)) = param.split_once('=') else { continue };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "databasename" | "database" => config.database = value.to_string(),
            "user" | "username" => config.user = Some(value.to_string()).filter(|u| !u.is_empty()),
            "instance" | "instancename" if !value.is_empty() => config.host = format!("{}\\{value}", config.host),
            "portnumber" | "port" => config.port = value.parse().ok().or(config.port),
            "servername" if host.is_empty() => config.host = value.to_string(),
            "encrypt" => encrypt = Some(value.to_ascii_lowercase()),
            "trustservercertificate" => trust = Some(value.eq_ignore_ascii_case("true")),
            _ => {}
        }
    }
    if encrypt.is_some() || trust.is_some() {
        config.ssl_mode = sql_server_ssl_mode(encrypt.as_deref(), trust).unwrap_or_default();
    }
    Some(config)
}

/// SSL settings from the SSL handler or the driver properties, if DBeaver has any.
fn ssl_mode(kind: DatabaseKind, conf: &Json) -> Option<SslMode> {
    let handlers = conf["handlers"].as_object();
    let ssl_handler = handlers.and_then(|h| h.iter().find(|(k, _)| k.contains("ssl")).map(|(_, v)| v));
    let props = &conf["properties"];
    let prop = |key: &str| props[key].as_str().map(str::to_ascii_lowercase);
    let parse = |mode: &str| match mode.replace('_', "-").as_str() {
        "disable" | "disabled" => Some(SslMode::Disable),
        "allow" | "prefer" | "preferred" => Some(SslMode::Prefer),
        "require" | "required" => Some(SslMode::Require),
        "verify-ca" | "verify-full" | "verify-identity" => Some(SslMode::VerifyFull),
        _ => None,
    };
    if kind == DatabaseKind::SqlServer {
        // "Trust server certificate" is a provider property (or the driver's own property). With
        // nothing set DBeaver sends `encrypt=false`; we keep our default (`prefer`), which encrypts
        // without verifying, rather than downgrade.
        let handler_on = ssl_handler.is_some_and(|h| h["enabled"].as_bool() == Some(true));
        let encrypt = prop("encrypt").or(handler_on.then(|| "true".into()));
        let flag = |v: &Json| v.as_str().map(|s| s.eq_ignore_ascii_case("true")).or(v.as_bool());
        let trust = flag(&conf["provider-properties"]["sslTrustServerCertificate"]).or(flag(&props["trustServerCertificate"]));
        if encrypt.is_none() && trust.is_none() {
            return None;
        }
        return sql_server_ssl_mode(encrypt.as_deref(), trust).ok();
    }
    if let Some(handler) = ssl_handler {
        if handler["enabled"].as_bool() == Some(true) {
            let hp = &handler["properties"];
            let mode = hp["sslMode"].as_str().or(hp["sslmode"].as_str()).and_then(|m| parse(&m.to_ascii_lowercase()));
            let verify = hp["ssl.verify.server"].as_bool().or(hp["verifyServerCert"].as_bool()) == Some(true);
            return Some(mode.unwrap_or(if verify { SslMode::VerifyFull } else { SslMode::Require }));
        }
    }
    match kind {
        DatabaseKind::Postgres => prop("sslmode").and_then(|m| parse(&m)),
        DatabaseKind::Mysql => prop("sslMode")
            .and_then(|m| parse(&m))
            .or_else(|| (prop("useSSL").as_deref() == Some("false")).then_some(SslMode::Disable)),
        DatabaseKind::Sqlite | DatabaseKind::Libsql | DatabaseKind::SqlServer => None,
    }
}

fn provider_display_name(provider: &str) -> String {
    match provider {
        "sqlserver" => "SQL Server".into(),
        "oracle" => "Oracle".into(),
        "db2" => "Db2".into(),
        "mongodb" => "MongoDB".into(),
        "redis" => "Redis".into(),
        "clickhouse" => "ClickHouse".into(),
        "snowflake" => "Snowflake".into(),
        "duckdb" => "DuckDB".into(),
        "" => "This database".into(),
        other => {
            let mut chars = other.chars();
            chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;
    use serde_json::json;

    fn encrypt(plain: &str) -> Vec<u8> {
        let iv = [7u8; 16];
        let mut out = iv.to_vec();
        out.extend(cbc::Encryptor::<aes::Aes128>::new_from_slices(&CREDENTIALS_KEY, &iv).unwrap().encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes()));
        out
    }

    fn sample() -> Json {
        json!({
            "folders": {"prod": {}},
            "connections": {
                "postgres-jdbc-1": {
                    "provider": "postgresql", "driver": "postgres-jdbc", "name": "Billing", "folder": "prod",
                    "save-password": true,
                    "configuration": {
                        "host": "db.example.com", "port": "5433", "database": "billing",
                        "url": "jdbc:postgresql://db.example.com:5433/billing", "configurationType": "MANUAL",
                        "provider-properties": {"@dbeaver-show-non-default-db@": "true"},
                        "properties": {"sslmode": "verify-full"},
                        "auth-model": "native"
                    }
                },
                "mysql8-2": {
                    "provider": "mysql", "driver": "mysql8", "name": "Shop", "save-password": true,
                    "configuration": {
                        "url": "jdbc:mysql://shop.internal:3307/shop?useSSL=false&serverTimezone=UTC", "configurationType": "URL",
                        "provider-properties": {"@dbeaver-show-all-dbs@": "false"},
                        "handlers": {"ssh_tunnel": {"type": "TUNNEL", "enabled": true, "save-password": true,
                            "properties": {"host": "bastion.example.com", "port": 2222, "authType": "PUBLIC_KEY", "keyPath": "/Users/me/.ssh/id_ed25519"}}}
                    }
                },
                "mariaDB-3": {
                    "provider": "mysql", "driver": "mariaDB", "name": "Legacy", "save-password": false,
                    "configuration": {"host": "maria", "port": "3306", "handlers": {"mysql_ssl": {"enabled": true, "properties": {}}}}
                },
                "sqlite_jdbc-4": {
                    "provider": "sqlite", "driver": "sqlite_jdbc", "name": "Notes",
                    "configuration": {"url": "jdbc:sqlite:/Users/me/notes.db", "configurationType": "URL"}
                },
                "libsql_jdbc-5": {"provider": "sqlite", "driver": "libsql_jdbc", "name": "Turso", "save-password": true,
                    "configuration": {"url": "jdbc:dbeaver:libsql:https://mydb-acme.turso.io", "configurationType": "URL", "auth-model": "libsql_token_jdbc"}},
                "libsql_jdbc-8": {"provider": "sqlite", "driver": "libsql_jdbc",
                    "configuration": {"server": "http://localhost:8080", "configurationType": "MANUAL"}},
                "libsql_jdbc-9": {"provider": "sqlite", "driver": "libsql_jdbc", "name": "Replica file",
                    "configuration": {"url": "jdbc:dbeaver:libsql:file:/Users/me/replica.db"}},
                "libsql_jdbc-10": {"provider": "sqlite", "driver": "libsql_jdbc", "name": "Empty", "configuration": {}},
                "azure-6": {
                    "provider": "sqlserver", "driver": "mssql_jdbc_azure", "name": "Azure", "save-password": true,
                    "configuration": {
                        "host": "acme.database.windows.net", "port": "1433", "database": "sales",
                        "auth-model": "sqlserver_ad_password",
                        "properties": {"encrypt": "true"}
                    }
                },
                "mssql_jdbc_ms_new-8": {
                    "provider": "sqlserver", "driver": "mssql_jdbc_ms_new", "name": "Warehouse", "save-password": true,
                    "configuration": {
                        "url": "jdbc:sqlserver://wh.internal\\SQLEXPRESS:14330;databaseName=dw;encrypt=true;trustServerCertificate=true",
                        "configurationType": "URL", "auth-model": "sqlserver_database",
                        "provider-properties": {"show-all-databases-azure": "false"}
                    }
                },
                "oracle-9": {"provider": "oracle", "driver": "oracle_thin", "name": "Ledger", "configuration": {}},
                "postgres-jdbc-7": {
                    "provider": "postgresql", "driver": "postgres-jdbc", "save-password": true,
                    "configuration": {"host": "localhost", "port": "5432", "database": "app"}
                }
            }
        })
    }

    #[test]
    fn decrypts_credentials() {
        let file = encrypt(r##"{"postgres-jdbc-1":{"#connection":{"user":"app","password":"s3cret"}},"mysql8-2":{"#connection":{"user":"root"},"network/ssh_tunnel":{"user":"deploy","password":"pw"}}}"##);
        let creds = decrypt_credentials(&file).unwrap();
        assert_eq!(creds["postgres-jdbc-1"], Credentials { user: Some("app".into()), password: Some("s3cret".into()), ..Default::default() });
        assert_eq!(creds["mysql8-2"].password, None);
        assert_eq!((creds["mysql8-2"].ssh_user.as_deref(), creds["mysql8-2"].ssh_password.as_deref()), (Some("deploy"), Some("pw")));
        assert!(decrypt_credentials(b"too short").is_none());
    }

    #[test]
    fn converts_supported_connections_and_explains_the_rest() {
        let creds = HashMap::from([
            ("postgres-jdbc-1".to_string(), Credentials { user: Some("app".into()), password: Some("s3cret".into()), ..Default::default() }),
            ("mysql8-2".to_string(), Credentials {
                user: Some("root".into()),
                password: Some("pw".into()),
                ssh_user: Some("deploy".into()),
                ssh_password: Some("key-pass".into()),
            }),
            ("mariaDB-3".to_string(), Credentials { user: Some("old".into()), password: Some("ignored".into()), ..Default::default() }),
            ("libsql_jdbc-5".to_string(), Credentials { user: None, password: Some("eyJ.token".into()), ..Default::default() }),
        ]);
        let scan = parse_data_sources(&sample(), &creds, None);
        let by_name = |n: &str| scan.connections.iter().find(|c| c.config.name == n).unwrap_or_else(|| panic!("{n}"));

        let pg = by_name("Billing");
        assert_eq!((pg.config.kind, pg.config.host.as_str(), pg.config.port, pg.config.database.as_str()), (DatabaseKind::Postgres, "db.example.com", Some(5433), "billing"));
        assert_eq!((pg.config.user.as_deref(), pg.config.password.as_deref()), (Some("app"), Some("s3cret")));
        assert_eq!((pg.config.group.as_str(), pg.config.ssl_mode, pg.config.show_all_databases), ("prod", SslMode::VerifyFull, true));
        assert!(pg.warnings.is_empty(), "{:?}", pg.warnings);

        let my = by_name("Shop");
        assert_eq!((my.config.kind, my.config.host.as_str(), my.config.port, my.config.database.as_str()), (DatabaseKind::Mysql, "shop.internal", Some(3307), "shop"));
        assert_eq!((my.config.ssl_mode, my.config.show_all_databases), (SslMode::Disable, false));
        let tunnel = my.config.ssh.as_ref().expect("the SSH tunnel is imported");
        assert_eq!(
            (tunnel.host.as_str(), tunnel.port, tunnel.user.as_str(), tunnel.auth, tunnel.key_path.as_str(), tunnel.secret.as_deref()),
            ("bastion.example.com", Some(2222), "deploy", SshAuth::PrivateKey, "/Users/me/.ssh/id_ed25519", Some("key-pass"))
        );
        assert!(my.warnings.is_empty(), "{:?}", my.warnings);

        let maria = by_name("Legacy");
        assert_eq!((maria.config.password.as_deref(), maria.config.ssl_mode), (None, SslMode::Require));
        assert!(maria.warnings.iter().any(|w| w.contains("No saved password")));

        let notes = by_name("Notes");
        assert_eq!((notes.config.kind, notes.config.database.as_str()), (DatabaseKind::Sqlite, "/Users/me/notes.db"));

        // Unnamed connections get the usual default name.
        assert!(!by_name("app").config.show_all_databases);

        let turso = by_name("Turso");
        assert_eq!((turso.config.kind, turso.config.host.as_str(), turso.config.ssl_mode), (DatabaseKind::Libsql, "mydb-acme.turso.io", SslMode::VerifyFull));
        assert_eq!((turso.config.password.as_deref(), turso.config.user.as_deref()), (Some("eyJ.token"), None));
        assert!(turso.warnings.is_empty(), "{:?}", turso.warnings);
        let local = by_name("localhost");
        assert_eq!((local.config.kind, local.config.port, local.config.ssl_mode), (DatabaseKind::Libsql, Some(8080), SslMode::Disable));
        assert_eq!(local.warnings, ["No saved auth token."]);
        let file = by_name("Replica file");
        assert_eq!((file.config.kind, file.config.database.as_str()), (DatabaseKind::Sqlite, "/Users/me/replica.db"));

        let azure = by_name("Azure");
        assert_eq!((azure.config.kind, azure.config.host.as_str(), azure.config.database.as_str()), (DatabaseKind::SqlServer, "acme.database.windows.net", "sales"));
        // encrypt=true without trusting the certificate: verify it. Azure shows one database by default.
        assert_eq!((azure.config.ssl_mode, azure.config.show_all_databases), (SslMode::VerifyFull, false));
        assert!(azure.warnings.iter().any(|w| w.contains("sqlserver_ad_password")), "{:?}", azure.warnings);

        let wh = by_name("Warehouse");
        assert_eq!((wh.config.host.as_str(), wh.config.port, wh.config.database.as_str()), ("wh.internal\\SQLEXPRESS", Some(14330), "dw"));
        assert_eq!((wh.config.ssl_mode, wh.config.show_all_databases), (SslMode::Require, false));
        assert!(!wh.warnings.iter().any(|w| w.contains("authentication")), "{:?}", wh.warnings);

        let skipped: Vec<_> = scan.skipped.iter().map(|s| (s.name.as_str(), s.reason.as_str())).collect();
        assert_eq!(skipped, [("Empty", "No server URL is set."), ("Ledger", "Oracle isn’t supported yet.")]);
    }

    #[test]
    fn scans_a_dbeaver_folder() {
        let dir = tempfile::tempdir().unwrap();
        let dot = dir.path().join("workspace6/Work/.dbeaver");
        std::fs::create_dir_all(&dot).unwrap();
        std::fs::write(dot.join("data-sources.json"), sample().to_string()).unwrap();
        std::fs::write(dot.join("credentials-config.json"), encrypt(r##"{"postgres-jdbc-1":{"#connection":{"user":"app","password":"s3cret"}}}"##)).unwrap();

        let scan = scan_path(dir.path()).unwrap();
        assert_eq!((scan.connections.len(), scan.skipped.len()), (10, 2));
        let pg = scan.connections.iter().find(|c| c.config.name == "Billing").unwrap();
        assert_eq!((pg.config.password.as_deref(), pg.config.group.as_str()), (Some("s3cret"), "Work / prod"));

        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(scan_path(empty.path()), Err(Error::InvalidConfig(_))));
    }
}
