//! D1 databases declared in a Workers project: the `d1_databases` of its wrangler config
//! (`wrangler.json`, `wrangler.jsonc` or `wrangler.toml`, environments included), with the local
//! copy `wrangler dev` keeps when there is one.
//!
//! Local databases are plain SQLite files at
//! `.wrangler/state/v3/d1/miniflare-D1DatabaseObject/<id>.sqlite`, where `<id>` is the Durable
//! Object ID Miniflare derives from the database's ID ([`local_file_name`]).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value as Json;

use crate::driver::{Error, Result};
use crate::model::{ConnectionConfig, DatabaseKind};

/// Config files wrangler reads, in its order of preference.
pub const CONFIG_FILES: [&str; 3] = ["wrangler.json", "wrangler.jsonc", "wrangler.toml"];

/// Where `wrangler dev` keeps local D1 databases, relative to the project.
pub const LOCAL_DIR: &str = ".wrangler/state/v3/d1/miniflare-D1DatabaseObject";

/// One `d1_databases` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WranglerDatabase {
    /// The wrangler config it comes from.
    pub config_file: PathBuf,
    /// `None` for the top level, else the `env.<name>` it's declared in.
    pub environment: Option<String>,
    pub binding: String,
    pub database_name: String,
    pub database_id: Option<String>,
    pub preview_database_id: Option<String>,
    /// The config's `account_id`, if it sets one.
    pub account_id: Option<String>,
    /// The local copy, if `wrangler dev` (or `wrangler d1 … --local`) made one.
    pub local_file: Option<PathBuf>,
}

impl WranglerDatabase {
    /// The remote database (by ID when known, else by name). The token is the caller's to supply.
    pub fn remote_config(&self, account_id: Option<&str>, token: Option<String>) -> ConnectionConfig {
        let mut config = ConnectionConfig::new_empty(DatabaseKind::D1);
        config.host = account_id.or(self.account_id.as_deref()).unwrap_or_default().trim().to_string();
        // Placeholder IDs (`"database_id": "local"`) aren't IDs: go by name then.
        config.database = self.database_id.clone().filter(|id| super::url::is_uuid(id)).unwrap_or_else(|| self.database_name.clone());
        config.password = token;
        config.name = self.database_name.clone();
        config
    }

    /// The local copy as a SQLite connection.
    pub fn local_config(&self) -> Option<ConnectionConfig> {
        let file = self.local_file.as_ref()?;
        let mut config = ConnectionConfig::new_empty(DatabaseKind::Sqlite);
        config.database = file.to_string_lossy().into_owned();
        config.name = format!("{} (local)", self.database_name);
        Some(config)
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    d1_databases: Vec<RawDatabase>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, RawEnv>,
}

#[derive(Debug, Default, Deserialize)]
struct RawEnv {
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    d1_databases: Vec<RawDatabase>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDatabase {
    #[serde(default)]
    binding: String,
    #[serde(default)]
    database_name: Option<String>,
    #[serde(default)]
    database_id: Option<String>,
    #[serde(default)]
    preview_database_id: Option<String>,
}

/// The D1 databases of the wrangler project in `dir` (none without a wrangler config).
pub fn find(dir: &Path) -> Result<Vec<WranglerDatabase>> {
    let Some(file) = CONFIG_FILES.iter().map(|name| dir.join(name)).find(|p| p.is_file()) else { return Ok(Vec::new()) };
    let text = std::fs::read_to_string(&file).map_err(|e| Error::InvalidConfig(format!("Couldn’t read {}: {e}", file.display())))?;
    let mut databases = parse(&text, is_toml(&file)).map_err(|e| Error::InvalidConfig(format!("{}: {e}", file.display())))?;
    let local = dir.join(LOCAL_DIR);
    for db in &mut databases {
        db.config_file = file.clone();
        let file = local_candidates(db).map(|id| local.join(local_file_name(&id))).find(|p| p.is_file());
        db.local_file = file;
    }
    Ok(databases)
}

fn is_toml(file: &Path) -> bool {
    file.extension().is_some_and(|e| e.eq_ignore_ascii_case("toml"))
}

/// The IDs wrangler may have keyed the local copy by, in its order: the preview ID, the ID, the binding.
fn local_candidates(db: &WranglerDatabase) -> impl Iterator<Item = String> + '_ {
    [db.preview_database_id.as_deref(), db.database_id.as_deref(), Some(db.binding.as_str())]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Parses a wrangler config (`toml`: TOML, else JSON with comments and trailing commas).
/// `config_file` and `local_file` are left empty.
pub fn parse(text: &str, toml: bool) -> std::result::Result<Vec<WranglerDatabase>, String> {
    let raw: RawConfig = if toml {
        toml::from_str(text).map_err(|e| e.to_string())?
    } else {
        let json: Json = serde_json::from_str(&strip_jsonc(text)).map_err(|e| e.to_string())?;
        serde_json::from_value(json).map_err(|e| e.to_string())?
    };
    let entry = |db: RawDatabase, environment: Option<&str>, account: Option<&String>| WranglerDatabase {
        config_file: PathBuf::new(),
        environment: environment.map(String::from),
        database_name: db.database_name.filter(|n| !n.is_empty()).unwrap_or_else(|| db.binding.clone()),
        binding: db.binding,
        database_id: db.database_id.filter(|s| !s.is_empty()),
        preview_database_id: db.preview_database_id.filter(|s| !s.is_empty()),
        account_id: account.filter(|s| !s.is_empty()).cloned(),
        local_file: None,
    };
    let mut out: Vec<WranglerDatabase> = raw.d1_databases.into_iter().map(|db| entry(db, None, raw.account_id.as_ref())).collect();
    for (name, env) in raw.env {
        let account = env.account_id.as_ref().or(raw.account_id.as_ref());
        out.extend(env.d1_databases.into_iter().map(|db| entry(db, Some(&name), account)));
    }
    Ok(out)
}

/// JSONC → JSON: drops `//` and `/* */` comments and trailing commas, leaving strings alone.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push(c);
                while let Some(c) = chars.next() {
                    out.push(c);
                    match c {
                        '\\' => out.extend(chars.next()),
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    // Trailing commas: a comma followed only by whitespace before `}` or `]`.
    let mut cleaned = String::with_capacity(out.len());
    let mut in_string = false;
    let mut escaped = false;
    let bytes: Vec<char> = out.chars().collect();
    for (i, &c) in bytes.iter().enumerate() {
        if in_string {
            cleaned.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        }
        if c == ',' && bytes[i + 1..].iter().find(|c| !c.is_whitespace()).is_some_and(|c| *c == '}' || *c == ']') {
            continue;
        }
        cleaned.push(c);
    }
    cleaned
}

/// The file name Miniflare stores a D1 database under: its Durable Object ID for `id` (the
/// database ID, or the binding when the config has none) in the `miniflare-D1DatabaseObject`
/// namespace, then `.sqlite`.
pub fn local_file_name(id: &str) -> String {
    use ring::{digest, hmac};
    let key = digest::digest(&digest::SHA256, b"miniflare-D1DatabaseObject");
    let key = hmac::Key::new(hmac::HMAC_SHA256, key.as_ref());
    let name = hmac::sign(&key, id.as_bytes());
    let name = &name.as_ref()[..16];
    let check = hmac::sign(&key, name);
    let hex: String = name.iter().chain(&check.as_ref()[..16]).map(|b| format!("{b:02x}")).collect();
    format!("{hex}.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_local_files_like_miniflare() {
        // From real `wrangler dev` state folders.
        assert_eq!(
            local_file_name("b6532fe4-d2a1-42b2-b783-00d97405614b"),
            "4239bc0fa0c43e8347d97afc965928ddd777e808abc4872ae3195fbfe9d3c799.sqlite"
        );
        assert_eq!(
            local_file_name("1d0ce556-134e-4802-84e5-93e255344d50"),
            "5c96efa3d55f68c005096d227a6b856df03418bbbaf8329be0e9ebc601a53b52.sqlite"
        );
    }

    #[test]
    fn parses_jsonc() {
        let text = r#"{
            // comment with "quotes" and a trailing comma,
            "name": "app", /* block
            comment */ "account_id": "f12da7851e4dd1d107a80417a1d4cbbd",
            "d1_databases": [
                { "binding": "DB", "database_name": "auth", "database_id": "b6532fe4-d2a1-42b2-b783-00d97405614b", "x": "a // not a comment," },
            ],
            "env": { "staging": { "d1_databases": [{ "binding": "DB", "database_name": "auth-staging" }] } },
        }"#;
        let dbs = parse(text, false).unwrap();
        assert_eq!(dbs.len(), 2);
        assert_eq!((dbs[0].binding.as_str(), dbs[0].database_name.as_str()), ("DB", "auth"));
        assert_eq!(dbs[0].database_id.as_deref(), Some("b6532fe4-d2a1-42b2-b783-00d97405614b"));
        assert_eq!(dbs[0].account_id.as_deref(), Some("f12da7851e4dd1d107a80417a1d4cbbd"));
        assert_eq!((dbs[1].environment.as_deref(), dbs[1].database_id.as_deref()), (Some("staging"), None));
        assert_eq!(dbs[1].account_id.as_deref(), Some("f12da7851e4dd1d107a80417a1d4cbbd"));

        let remote = dbs[0].remote_config(None, Some("tok".into()));
        assert_eq!((remote.kind, remote.database.as_str(), remote.name.as_str()), (DatabaseKind::D1, "b6532fe4-d2a1-42b2-b783-00d97405614b", "auth"));
        assert_eq!(dbs[1].remote_config(None, None).database, "auth-staging");
        let placeholder = parse(r#"{"d1_databases": [{"binding": "DB", "database_name": "juani", "database_id": "local"}]}"#, false).unwrap();
        assert_eq!(placeholder[0].remote_config(None, None).database, "juani");
    }

    #[test]
    fn parses_toml() {
        let text = r#"
name = "app"
[[d1_databases]]
binding = "DB"
database_name = "notes"
database_id = "fda15bbf-40c6-4d92-9766-5921e932fdd6"

[env.production]
account_id = "f12da7851e4dd1d107a80417a1d4cbbd"
[[env.production.d1_databases]]
binding = "DB"
database_name = "notes-prod"
"#;
        let dbs = parse(text, true).unwrap();
        assert_eq!(dbs.iter().map(|d| d.database_name.as_str()).collect::<Vec<_>>(), ["notes", "notes-prod"]);
        assert_eq!((dbs[0].account_id.as_deref(), dbs[1].account_id.as_deref()), (None, Some("f12da7851e4dd1d107a80417a1d4cbbd")));
    }

    #[test]
    fn finds_local_copies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("wrangler.jsonc"),
            r#"{"d1_databases": [{"binding": "DB", "database_name": "auth", "database_id": "b6532fe4-d2a1-42b2-b783-00d97405614b"}]}"#,
        )
        .unwrap();
        assert_eq!(find(dir.path()).unwrap()[0].local_file, None);
        let local = dir.path().join(LOCAL_DIR);
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join(local_file_name("b6532fe4-d2a1-42b2-b783-00d97405614b")), b"").unwrap();
        let found = find(dir.path()).unwrap();
        assert!(found[0].local_file.as_ref().is_some_and(|p| p.is_file()));
        let config = found[0].local_config().unwrap();
        assert_eq!((config.kind, config.name.as_str()), (DatabaseKind::Sqlite, "auth (local)"));
        assert!(find(&dir.path().join("missing")).unwrap().is_empty());
    }
}
