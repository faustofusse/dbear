//! Cloudflare D1 connection URLs: `d1://<account id>/<database>?token=…`.
//!
//! Mapping onto [`ConnectionConfig`] (no D1-specific fields):
//! - `host`: the Cloudflare account ID (32 hex digits). May be empty when the token reaches a
//!   single account: the driver asks Cloudflare which one.
//! - `database`: the database's name (as in `wrangler.toml`'s `database_name`) or its ID (a UUID).
//! - `password`: the API token (kept in the platform keychain like any password). It needs the
//!   "D1 Read" permission to browse, "D1 Edit" to change anything.
//! - `port`, `user` and `ssl_mode` are unused: requests go to `api.cloudflare.com` over verified TLS.

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use url::Url;

use crate::driver::{Error, Result};
use crate::model::{ConnectionConfig, DatabaseKind};

/// Parses `d1://<account id>/<database>[?token=…]`. The account can be left out (`d1:///<database>`)
/// or given as `?account=`; the token also reads `apiToken`, `api_token` or the userinfo's password.
pub(crate) fn from_url(url: &Url) -> Result<ConnectionConfig> {
    let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
    let mut config = ConnectionConfig::new_empty(DatabaseKind::D1);
    config.host = url.host_str().map(decode).unwrap_or_default();
    config.database = decode(url.path().trim_matches('/'));
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "token" | "apiToken" | "api_token" => config.password = Some(value.into_owned()).filter(|t| !t.is_empty()),
            "account" | "accountId" | "account_id" if config.host.is_empty() => config.host = value.into_owned(),
            _ => {}
        }
    }
    if config.password.is_none() {
        config.password = url.password().map(decode).filter(|t| !t.is_empty());
    }
    if config.database.is_empty() {
        return Err(Error::InvalidConfig("The URL has no database: use d1://<account id>/<database name or ID>.".into()));
    }
    config.name = config.default_name();
    Ok(config)
}

/// Everything but URL-safe characters (RFC 3986's unreserved), so `my-app-db` stays readable.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// `d1://<account id>/<database>[?token=…]`. The token is only included when asked for.
pub(crate) fn to_url(config: &ConnectionConfig, include_token: bool) -> String {
    let encode = |s: &str| utf8_percent_encode(s.trim(), COMPONENT).to_string();
    let mut url = format!("d1://{}/{}", encode(&config.host), encode(&config.database));
    if let (true, Some(token)) = (include_token, config.password.as_deref().map(str::trim).filter(|t| !t.is_empty())) {
        url.push_str(&format!("?token={}", encode(token)));
    }
    url
}

/// The name shown when the user leaves it empty: the database's name, or the start of its ID.
pub(crate) fn default_name(database: &str) -> String {
    let database = database.trim();
    if is_uuid(database) {
        format!("D1 {}", &database[..8])
    } else {
        database.to_string()
    }
}

/// D1 database IDs are UUIDs (`fda15bbf-40c6-4d92-9766-5921e932fdd6`); anything else is a name.
pub(crate) fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups.iter().all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
}

pub(crate) fn validate(config: &ConnectionConfig) -> Result<()> {
    let invalid = |msg: &str| Err(Error::InvalidConfig(msg.into()));
    let account = config.host.trim();
    if !account.is_empty() && (account.len() != 32 || !account.chars().all(|c| c.is_ascii_hexdigit())) {
        return invalid("The account ID is 32 hexadecimal characters (Cloudflare dashboard ▸ Workers & Pages ▸ Account details).");
    }
    let database = config.database.trim();
    if database.is_empty() {
        return invalid("Enter the database’s name or ID.");
    }
    if database.contains(['/', '?', '#']) {
        return invalid("Enter just the database’s name or ID (paste full URLs in the URL field).");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "f12da7851e4dd1d107a80417a1d4cbbd";
    const ID: &str = "fda15bbf-40c6-4d92-9766-5921e932fdd6";

    fn parse(url: &str) -> ConnectionConfig {
        ConnectionConfig::from_url(url).unwrap()
    }

    #[test]
    fn parses_urls() {
        let c = parse(&format!("d1://{ACCOUNT}/my-app-db?token=abc%2Bdef"));
        assert_eq!(c.kind, DatabaseKind::D1);
        assert_eq!((c.host.as_str(), c.database.as_str(), c.password.as_deref()), (ACCOUNT, "my-app-db", Some("abc+def")));
        assert_eq!((c.name.as_str(), c.port, c.user.as_deref()), ("my-app-db", None, None));
        assert_eq!(c.summary(), "Cloudflare D1 · my-app-db");
        assert!(c.show_all_databases && c.supports_multiple_databases() && !c.supports_ssh());

        let by_id = parse(&format!("d1://{ACCOUNT}/{ID}"));
        assert_eq!((by_id.database.as_str(), by_id.name.as_str(), by_id.password), (ID, "D1 fda15bbf", None));

        let no_account = parse("d1:///notes?token=t");
        assert_eq!((no_account.host.as_str(), no_account.database.as_str()), ("", "notes"));
        assert_eq!(parse(&format!("d1:///notes?account={ACCOUNT}")).host, ACCOUNT);
        assert_eq!(parse(&format!("d1://:tok@{ACCOUNT}/notes")).password.as_deref(), Some("tok"));
        assert!(ConnectionConfig::from_url(&format!("d1://{ACCOUNT}")).is_err());
    }

    #[test]
    fn formats_urls() {
        let c = parse(&format!("d1://{ACCOUNT}/my%20db?token=a%2Bb"));
        assert_eq!(c.database, "my db");
        assert_eq!(c.to_url(false), format!("d1://{ACCOUNT}/my%20db"));
        assert_eq!(c.to_url(true), format!("d1://{ACCOUNT}/my%20db?token=a%2Bb"));
        assert_eq!(parse(&c.to_url(true)), c);
        let id = parse(&format!("d1://{ACCOUNT}/{ID}"));
        assert_eq!(id.to_url(false), format!("d1://{ACCOUNT}/{ID}"));
        assert_eq!(parse("d1:///my-app_db.v2").to_url(false), "d1:///my-app_db.v2");
        let no_account = parse("d1:///notes");
        assert_eq!(parse(&no_account.to_url(false)), no_account);
    }

    #[test]
    fn validates() {
        let mut c = ConnectionConfig::new_empty(DatabaseKind::D1);
        assert_eq!((c.host.as_str(), c.ssl_mode), ("", crate::model::SslMode::VerifyFull));
        assert!(c.validate().is_err());
        c.database = "notes".into();
        assert!(c.validate().is_ok());
        c.host = "not-an-account".into();
        assert!(c.validate().is_err());
        c.host = ACCOUNT.into();
        assert!(c.validate().is_ok());
        c.database = "https://example.com/x".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn tells_ids_from_names() {
        assert!(is_uuid(ID));
        assert!(!is_uuid("my-app-db") && !is_uuid("fda15bbf-40c6-4d92-9766-5921e932fdd") && !is_uuid("zda15bbf-40c6-4d92-9766-5921e932fdd6"));
    }
}
