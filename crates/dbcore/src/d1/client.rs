//! A small client for D1's REST API (`api.cloudflare.com/client/v4`).
//!
//! SQL goes to `POST /accounts/{account}/d1/database/{id}/raw`, which answers rows as arrays. One
//! request carries one statement or a `batch` of them; D1 runs a request's statements as one
//! transaction (a failing statement rolls back the whole request) and answers with a single error
//! that doesn't say which statement failed. `BEGIN`/`COMMIT` are refused: a request is the only
//! transaction there is.
//!
//! Values come back as JSON, so integers past 2^53 lose precision, reals that happen to be whole
//! read as integers and blobs arrive as arrays of bytes. The driver's own queries select `quote(…)`
//! of each column instead and get exact SQL literals back ([`parse_literal`]).
//!
//! Parameters are sent as JSON too and numbers bind as REAL, so only text is ever bound; other
//! values are written into the SQL as literals.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use base64::Engine as _;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::dialect::error_chain;
use crate::driver::{Error, Result};
use crate::libsql::hrana::{Col, HValue, StmtResult, BASE64};
use crate::model::{ConnectionConfig, SslMode};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Cloudflare's API. `DBEAR_D1_API` points the client at another server (tests).
fn api_base() -> String {
    std::env::var("DBEAR_D1_API").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://api.cloudflare.com/client/v4".into())
}

/// A failed request: the error, and for SQL errors the byte offset D1 reports in the statement.
#[derive(Debug)]
pub(crate) struct SqlFailure {
    pub error: Error,
    pub offset: Option<usize>,
}

impl SqlFailure {
    fn new(error: Error) -> Self {
        Self { error, offset: None }
    }
}

type Answer<T> = std::result::Result<T, SqlFailure>;

/// A statement and its text parameters (`?1`, `?2`…).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct D1Stmt {
    pub sql: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<String>,
}

impl D1Stmt {
    pub fn new(sql: impl Into<String>) -> Self {
        Self { sql: sql.into(), params: Vec::new() }
    }

    pub fn with_args(sql: impl Into<String>, args: &[&str]) -> Self {
        Self { sql: sql.into(), params: args.iter().map(|a| (*a).to_string()).collect() }
    }
}

#[derive(Serialize)]
struct BatchBody<'a> {
    batch: &'a [D1Stmt],
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    result: Option<T>,
    #[serde(default)]
    result_info: Option<ResultInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ApiError {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct ResultInfo {
    #[serde(default)]
    total_count: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct RawResult {
    #[serde(default)]
    results: Option<RawRows>,
    #[serde(default)]
    meta: Option<RawMeta>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRows {
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    rows: Vec<Vec<Json>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMeta {
    #[serde(default)]
    changes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DatabaseInfo {
    pub uuid: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
struct Account {
    id: String,
    #[serde(default)]
    name: String,
}

/// The HTTP client for one connection. The account and database IDs are looked up on first use
/// when the config gives a name (or no account).
pub(crate) struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
    account: String,
    database: String,
    resolved: Mutex<Option<(String, String)>>,
}

impl Client {
    pub fn new(config: &ConnectionConfig) -> Result<Self> {
        let token = config.password.as_deref().map(str::trim).unwrap_or_default().to_string();
        if token.is_empty() {
            return Err(Error::ConnectionFailed("Enter a Cloudflare API token with the D1 permission.".into()));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent("dbear")
            .use_preconfigured_tls(crate::libsql::hrana::tls_config(SslMode::VerifyFull)?)
            .build()
            .map_err(|e| Error::Internal(format!("HTTP client: {}", error_chain(&e))))?;
        Ok(Self {
            http,
            base: api_base(),
            token,
            account: config.host.trim().to_string(),
            database: config.database.trim().to_string(),
            resolved: Mutex::new(None),
        })
    }

    /// Runs statements in one request (one transaction); one result each.
    pub async fn run(&self, statements: &[D1Stmt]) -> Answer<Vec<StmtResult>> {
        if statements.is_empty() {
            return Ok(Vec::new());
        }
        let (account, id) = self.ids().await?;
        let body = match statements {
            [one] => serde_json::to_vec(one),
            many => serde_json::to_vec(&BatchBody { batch: many }),
        }
        .map_err(|e| SqlFailure::new(Error::Internal(e.to_string())))?;
        let url = format!("{}/accounts/{account}/d1/database/{id}/raw", self.base);
        let results: Vec<RawResult> = self.send(self.http.post(url).header(reqwest::header::CONTENT_TYPE, "application/json").body(body)).await?.0;
        Ok(results.into_iter().map(into_stmt_result).collect())
    }

    /// Every database of the account, by name.
    pub async fn list_databases(&self) -> Answer<Vec<DatabaseInfo>> {
        let account = self.account_id().await?;
        let mut all: Vec<DatabaseInfo> = Vec::new();
        for page in 1.. {
            let url = format!("{}/accounts/{account}/d1/database?per_page=100&page={page}", self.base);
            let (batch, info): (Vec<DatabaseInfo>, _) = self.send(self.http.get(url)).await?;
            let done = batch.len() < 100 || info.and_then(|i| i.total_count).is_some_and(|total| all.len() + batch.len() >= total as usize);
            all.extend(batch);
            if done || page >= 100 {
                break;
            }
        }
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all)
    }

    /// The account and database IDs: as configured, or looked up (once).
    async fn ids(&self) -> Answer<(String, String)> {
        if let Some(ids) = self.resolved.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return Ok(ids);
        }
        let account = self.account_id().await?;
        let id = if super::url::is_uuid(&self.database) {
            self.database.clone()
        } else {
            let url = format!("{}/accounts/{account}/d1/database", self.base);
            let (found, _): (Vec<DatabaseInfo>, _) = self.send(self.http.get(url).query(&[("name", self.database.as_str())])).await?;
            match found.into_iter().find(|d| d.name == self.database) {
                Some(db) => db.uuid,
                None => {
                    let error = Error::ConnectionFailed(format!("This account has no D1 database named “{}”.", self.database));
                    return Err(SqlFailure::new(error));
                }
            }
        };
        let ids = (account, id);
        *self.resolved.lock().unwrap_or_else(|p| p.into_inner()) = Some(ids.clone());
        Ok(ids)
    }

    async fn account_id(&self) -> Answer<String> {
        if !self.account.is_empty() {
            return Ok(self.account.clone());
        }
        if let Some((account, _)) = self.resolved.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return Ok(account);
        }
        let url = format!("{}/accounts?per_page=50", self.base);
        let (accounts, _): (Vec<Account>, _) = self.send(self.http.get(url)).await?;
        match accounts.as_slice() {
            [one] => Ok(one.id.clone()),
            [] => Err(SqlFailure::new(Error::ConnectionFailed("The API token can’t see any account: enter the account ID.".into()))),
            many => {
                let names: Vec<String> = many.iter().map(|a| format!("{} ({})", a.name, a.id)).collect();
                let message = format!("The API token reaches several accounts; enter the account ID: {}.", names.join(", "));
                Err(SqlFailure::new(Error::ConnectionFailed(message)))
            }
        }
    }

    async fn send<T: for<'de> Deserialize<'de>>(&self, request: reqwest::RequestBuilder) -> Answer<(T, Option<ResultInfo>)> {
        let transport = |e: reqwest::Error| SqlFailure::new(Error::ConnectionFailed(error_chain(&e)));
        let response = request.bearer_auth(&self.token).send().await.map_err(transport)?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(transport)?;
        match serde_json::from_slice::<Envelope<T>>(&body) {
            Ok(Envelope { success: true, result: Some(result), result_info, .. }) => Ok((result, result_info)),
            Ok(envelope) => Err(failure(status, &envelope.errors)),
            Err(e) => {
                // Not the API's JSON (a proxy's page, or a shape this client doesn't expect).
                let text: String = String::from_utf8_lossy(&body).trim().chars().take(300).collect();
                let errors: Vec<ApiError> = serde_json::from_slice::<Envelope<Json>>(&body).map(|e| e.errors).unwrap_or_default();
                if !errors.is_empty() || status >= 400 {
                    let errors = if errors.is_empty() { vec![ApiError { code: 0, message: text }] } else { errors };
                    return Err(failure(status, &errors));
                }
                Err(SqlFailure::new(Error::Internal(format!("Unexpected answer from D1 ({e}): {text}"))))
            }
        }
    }
}

fn failure(status: u16, errors: &[ApiError]) -> SqlFailure {
    let offset = errors.first().filter(|e| e.code == 7500).and_then(|e| error_offset(&e.message));
    SqlFailure { error: api_error(status, errors), offset }
}

/// An error answer as a dbear error: SQL errors become `ERROR: …` (the query's fault), the rest
/// connection errors.
pub(crate) fn api_error(status: u16, errors: &[ApiError]) -> Error {
    let message = errors.iter().map(|e| e.message.trim()).filter(|m| !m.is_empty()).collect::<Vec<_>>().join("; ");
    let message = if message.is_empty() { format!("HTTP {status}") } else { message };
    let code = errors.first().map_or(0, |e| e.code);
    match status {
        401 | 403 => Error::ConnectionFailed(format!(
            "Cloudflare rejected the API token (HTTP {status}): {message}. It needs the D1 Read permission (D1 Edit to change data) on this account."
        )),
        404 => Error::ConnectionFailed(format!("Not found: {message}")),
        429 => Error::Query(format!("ERROR: Cloudflare’s API rate limit was reached; try again in a minute. ({message})")),
        s if s >= 500 => Error::ConnectionFailed(format!("Cloudflare error (HTTP {s}): {message}")),
        _ if code == 7500 => Error::Query(format!("ERROR: {}", sql_message(&message))),
        _ => Error::Query(format!("ERROR: {message}")),
    }
}

/// D1 appends SQLite's result code (and the offset in the statement) to messages:
/// `near "selec": syntax error at offset 0: SQLITE_ERROR` → `near "selec": syntax error`.
pub(crate) fn sql_message(message: &str) -> String {
    static SUFFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?: at offset \d+)?: SQLITE_[A-Z_]+(?: \(extended: [A-Z_]+\))?$").expect("valid regex"));
    SUFFIX.replace(message.trim(), "").into_owned()
}

/// The offset D1 reports for an error in a statement (`… at offset 12: SQLITE_ERROR`).
pub(crate) fn error_offset(message: &str) -> Option<usize> {
    let rest = &message[message.rfind(" at offset ")? + 11..];
    rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

fn into_stmt_result(result: RawResult) -> StmtResult {
    let rows = result.results.unwrap_or_default();
    StmtResult {
        cols: rows.columns.into_iter().map(|name| Col { name: Some(name), decltype: None }).collect(),
        rows: rows.rows.into_iter().map(|row| row.into_iter().map(json_value).collect()).collect(),
        affected_row_count: result.meta.map_or(0, |m| m.changes),
    }
}

/// A JSON value from D1 as the closest SQLite value (see the module's notes on precision).
pub(crate) fn json_value(value: Json) -> HValue {
    match value {
        Json::Null => HValue::Null,
        Json::Bool(b) => HValue::Integer { value: i64::from(b).to_string() },
        Json::Number(n) if n.is_i64() || n.is_u64() => HValue::Integer { value: n.to_string() },
        Json::Number(n) => HValue::Float { value: n.as_f64().unwrap_or(f64::NAN) },
        Json::String(value) => HValue::Text { value },
        Json::Array(items) if items.iter().all(|i| i.as_u64().is_some_and(|b| b <= 255)) => {
            let bytes: Vec<u8> = items.iter().filter_map(Json::as_u64).map(|b| b as u8).collect();
            HValue::Blob { base64: BASE64.encode(bytes) }
        }
        other => HValue::Text { value: other.to_string() },
    }
}

/// A value from SQLite's `quote()`: `NULL`, `12`, `1.5`, `9.0e+999`, `'it''s'` or `X'00FF'`.
pub(crate) fn parse_literal(literal: &str) -> HValue {
    let s = literal.trim();
    if s.eq_ignore_ascii_case("null") {
        return HValue::Null;
    }
    if let Some(inner) = s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return HValue::Text { value: inner.replace("''", "'") };
    }
    if let Some(hex) = s.strip_prefix("X'").or_else(|| s.strip_prefix("x'")).and_then(|r| r.strip_suffix('\'')) {
        let bytes: Option<Vec<u8>> =
            (0..hex.len()).step_by(2).map(|i| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect();
        if let Some(bytes) = bytes {
            return HValue::Blob { base64: BASE64.encode(bytes) };
        }
    }
    if s.parse::<i64>().is_ok() {
        return HValue::Integer { value: s.to_string() };
    }
    match s.parse::<f64>() {
        Ok(value) => HValue::Float { value },
        // Not a literal (a column that wasn't quoted): keep it as text.
        Err(_) => HValue::Text { value: literal.to_string() },
    }
}

/// Rows of literals (from `quote()`) as values.
pub(crate) fn unquote_rows(result: &mut StmtResult) {
    for row in &mut result.rows {
        for value in row.iter_mut() {
            if let HValue::Text { value: literal } = value {
                *value = parse_literal(literal);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_raw_results() {
        let body = json!({"result": [{"results": {"columns": ["id", "b", "r"], "rows": [[1, [0, 255], 2.5], [9007199254740993u64, null, "x"]]},
            "success": true, "meta": {"changes": 3}}], "errors": [], "messages": [], "success": true});
        let envelope: Envelope<Vec<RawResult>> = serde_json::from_value(body).unwrap();
        let result = into_stmt_result(envelope.result.unwrap().pop().unwrap());
        assert_eq!(result.cols.iter().map(|c| c.name.clone().unwrap()).collect::<Vec<_>>(), ["id", "b", "r"]);
        assert_eq!(result.rows[0], [HValue::Integer { value: "1".into() }, HValue::Blob { base64: "AP8=".into() }, HValue::Float { value: 2.5 }]);
        assert_eq!(result.rows[1][0], HValue::Integer { value: "9007199254740993".into() });
        assert_eq!(result.affected_row_count, 3);
    }

    #[test]
    fn parses_quoted_literals() {
        assert_eq!(parse_literal("NULL"), HValue::Null);
        assert_eq!(parse_literal("-9223372036854775808"), HValue::Integer { value: "-9223372036854775808".into() });
        assert_eq!(parse_literal("1.0"), HValue::Float { value: 1.0 });
        assert_eq!(parse_literal("0.30000000000000004"), HValue::Float { value: 0.1 + 0.2 });
        assert_eq!(parse_literal("-9.0e+999"), HValue::Float { value: f64::NEG_INFINITY });
        assert_eq!(parse_literal("'it''s'"), HValue::Text { value: "it's".into() });
        assert_eq!(parse_literal("''"), HValue::Text { value: String::new() });
        assert_eq!(parse_literal("X'00FF10'"), HValue::Blob { base64: BASE64.encode([0, 255, 16]) });
        assert_eq!(parse_literal("X''"), HValue::Blob { base64: String::new() });
    }

    #[test]
    fn maps_errors() {
        let error = |status, code, message: &str| api_error(status, &[ApiError { code, message: message.into() }]);
        assert_eq!(error(400, 7500, "near \"selec\": syntax error at offset 0: SQLITE_ERROR"), Error::Query("ERROR: near \"selec\": syntax error".into()));
        assert_eq!(
            error(400, 7500, "UNIQUE constraint failed: t.id: SQLITE_CONSTRAINT (extended: SQLITE_CONSTRAINT_PRIMARYKEY)"),
            Error::Query("ERROR: UNIQUE constraint failed: t.id".into())
        );
        assert!(matches!(error(401, 10000, "Authentication error"), Error::ConnectionFailed(m) if m.contains("API token")));
        assert!(matches!(error(404, 7404, "The database x could not be found"), Error::ConnectionFailed(_)));
        assert!(matches!(error(429, 971, "Please wait"), Error::Query(m) if m.contains("rate limit")));
        assert_eq!(error_offset("near \"x\": syntax error at offset 12: SQLITE_ERROR"), Some(12));
        assert_eq!(error_offset("no such table: t: SQLITE_ERROR"), None);
    }
}
