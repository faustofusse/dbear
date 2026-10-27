//! A small Hrana 3 client over HTTP with JSON encoding (the protocol Turso and `sqld` speak).
//! Spec: <https://github.com/tursodatabase/libsql/blob/main/docs/HRANA_3_SPEC.md>
//!
//! Two endpoints are used:
//! - `POST v3/pipeline`: a list of requests (execute, batch, close…) on a stream, answered at once.
//! - `POST v3/cursor`: one batch whose results stream back as JSON lines, so a huge result can be
//!   counted without being held in memory.
//!
//! Streams are identified by a baton the server hands back with each response; [`Stream`] keeps it.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use rusqlite::types::ValueRef;
use serde::{Deserialize, Serialize};

use crate::dialect::error_chain;
use crate::driver::{Error, Result};
use crate::keyset::CursorValue;
use crate::model::{ConnectionConfig, SslMode, Value};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

// MARK: Requests

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StreamRequest {
    Execute { stmt: Stmt },
    Batch { batch: Batch },
    Close,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Stmt {
    pub sql: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<Arg>,
    pub want_rows: bool,
}

impl Stmt {
    pub fn new(sql: impl Into<String>) -> Self {
        Self { sql: sql.into(), args: Vec::new(), want_rows: true }
    }

    /// With positional (`?1`, `?2`…) text arguments.
    pub fn with_args(sql: impl Into<String>, args: &[&str]) -> Self {
        Self { args: args.iter().map(|a| Arg::Text { value: (*a).to_string() }).collect(), ..Self::new(sql) }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Arg {
    Null,
    Integer { value: String },
    Float { value: f64 },
    Text { value: String },
    Blob { base64: String },
}

impl Arg {
    /// A keyset cursor value, sent back with its storage class. Non-finite floats can't be
    /// sent as JSON: the caller writes those as literals instead.
    pub fn from_cursor(value: &CursorValue) -> Self {
        match value {
            CursorValue::Null => Self::Null,
            CursorValue::Int(i) => Self::Integer { value: i.to_string() },
            CursorValue::Float(bits) => Self::Float { value: f64::from_bits(*bits) },
            CursorValue::Text(t) => Self::Text { value: t.clone() },
            CursorValue::Bytes(b) => Self::Blob { base64: BASE64.encode(b) },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Batch {
    pub steps: Vec<BatchStep>,
}

impl Batch {
    /// Runs `statements` in order, each only if the one before succeeded (like a script stopping at
    /// its first error).
    pub fn chained(statements: impl IntoIterator<Item = Stmt>) -> Self {
        let steps = statements
            .into_iter()
            .enumerate()
            .map(|(i, stmt)| BatchStep { condition: i.checked_sub(1).map(|step| BatchCond::Ok { step: step as u32 }), stmt })
            .collect();
        Self { steps }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct BatchStep {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<BatchCond>,
    pub stmt: Stmt,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum BatchCond {
    Ok { step: u32 },
}

#[derive(Serialize)]
struct PipelineRequest<'a> {
    baton: Option<&'a str>,
    requests: &'a [StreamRequest],
}

#[derive(Serialize)]
struct CursorRequest<'a> {
    baton: Option<&'a str>,
    batch: &'a Batch,
}

// MARK: Responses

#[derive(Debug, Deserialize)]
struct PipelineResponse {
    baton: Option<String>,
    base_url: Option<String>,
    results: Vec<StreamResult>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StreamResult {
    Ok { response: StreamResponse },
    Error { error: HranaError },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StreamResponse {
    Execute { result: StmtResult },
    Batch { result: BatchResult },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct StmtResult {
    pub cols: Vec<Col>,
    pub rows: Vec<Vec<HValue>>,
    pub affected_row_count: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct BatchResult {
    pub step_results: Vec<Option<StmtResult>>,
    pub step_errors: Vec<Option<HranaError>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Col {
    pub name: Option<String>,
    pub decltype: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct HranaError {
    pub message: String,
    pub code: Option<String>,
}

impl HranaError {
    /// As shown to the user: `ERROR: no such table: x`. sqld prefixes SQLite's message with the
    /// error kind (`SQLite error: …`, `SQLITE_UNKNOWN: …`); that's noise in a grid.
    pub fn message(&self) -> String {
        let mut message = self.message.trim();
        for prefix in ["SQLite error:", "SQL string could not be parsed:"] {
            if let Some(rest) = message.strip_prefix(prefix) {
                message = rest.trim_start();
            }
        }
        format!("ERROR: {message}")
    }

    pub fn is_interrupt(&self) -> bool {
        self.code.as_deref().is_some_and(|c| c.contains("INTERRUPT")) || self.message.contains("interrupted")
    }

    pub fn into_error(self) -> Error {
        if self.is_interrupt() {
            Error::Cancelled
        } else {
            Error::Query(self.message())
        }
    }
}

/// A value on the wire. Integers are strings (64-bit safe), blobs base64.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum HValue {
    Null,
    Integer { value: String },
    Float { value: f64 },
    Text { value: String },
    Blob { base64: String },
}

impl HValue {
    /// Decoded like the SQLite driver does: the declared type (lowercase) refines bools and decimals.
    pub fn decode(&self, declared: &str) -> Value {
        match self {
            Self::Null => crate::sqlite::decode(ValueRef::Null, declared),
            Self::Integer { value } => match value.parse::<i64>() {
                Ok(i) => crate::sqlite::decode(ValueRef::Integer(i), declared),
                Err(_) => Value::Text(value.clone()),
            },
            Self::Float { value } => crate::sqlite::decode(ValueRef::Real(*value), declared),
            Self::Text { value } => crate::sqlite::decode(ValueRef::Text(value.as_bytes()), declared),
            Self::Blob { base64 } => {
                let bytes = BASE64.decode(base64.trim()).unwrap_or_default();
                crate::sqlite::decode(ValueRef::Blob(&bytes), declared)
            }
        }
    }

    /// The exact value (storage class included) of a sort key, for the next page's seek.
    pub fn to_cursor(&self) -> CursorValue {
        match self {
            Self::Null => CursorValue::Null,
            Self::Integer { value } => value.parse().map_or_else(|_| CursorValue::Text(value.clone()), CursorValue::Int),
            Self::Float { value } => CursorValue::float(*value),
            Self::Text { value } => CursorValue::Text(value.clone()),
            Self::Blob { base64 } => CursorValue::Bytes(BASE64.decode(base64.trim()).unwrap_or_default()),
        }
    }

    pub fn as_text(&self) -> Option<String> {
        match self {
            Self::Text { value } | Self::Integer { value } => Some(value.clone()),
            Self::Float { value } => Some(value.to_string()),
            Self::Null | Self::Blob { .. } => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Integer { value } | Self::Text { value } => value.parse().ok(),
            Self::Float { value } => Some(*value as i64),
            Self::Null | Self::Blob { .. } => None,
        }
    }
}

/// Servers differ on base64 padding; accept both.
pub(crate) const BASE64: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
);

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum CursorEntry {
    StepBegin {
        #[serde(default)]
        cols: Vec<Col>,
    },
    StepEnd {
        #[serde(default)]
        affected_row_count: u64,
    },
    StepError {
        step: u32,
        error: HranaError,
    },
    Row {
        row: Vec<HValue>,
    },
    Error {
        error: HranaError,
    },
    /// Entries this client doesn't use (sqld sends `replication_index`).
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct CursorHeader {
    baton: Option<String>,
    base_url: Option<String>,
}

// MARK: Client

/// The HTTP client for one connection: pooled, shared by every stream.
pub(crate) struct Client {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl Client {
    pub fn new(config: &ConnectionConfig) -> Result<Self> {
        // Always set (even for plain `http://`): without it reqwest would look for a default rustls provider.
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent("dbear")
            .use_preconfigured_tls(tls_config(config.ssl_mode)?)
            .build().map_err(|e| Error::Internal(format!("HTTP client: {}", error_chain(&e))))?;
        let token = config.password.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(String::from);
        Ok(Self { http, base_url: super::url::base_url(config), token })
    }

    /// A new stream; the server opens it with the first request.
    pub fn stream(self: &Arc<Self>) -> Stream {
        Stream { client: self.clone(), baton: None, base_url: None }
    }

    async fn post(&self, base_url: &str, endpoint: &str, body: Vec<u8>) -> Result<reqwest::Response> {
        let mut request = self
            .http
            .post(format!("{}/{endpoint}", base_url.trim_end_matches('/')))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(transport_error)?;
        check_status(response).await
    }
}

pub(crate) fn tls_config(mode: SslMode) -> Result<rustls::ClientConfig> {
    use crate::postgres::tls::{provider, AcceptAnyCert};
    let builder = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Internal(format!("TLS setup: {e}")))?;
    let mut config = match mode {
        // `Disable` never uses TLS (`http://`); verifying is the safe choice should it ever.
        SslMode::VerifyFull | SslMode::Disable => {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        // Encrypted, certificate not checked (a self-hosted sqld with a self-signed certificate).
        SslMode::Prefer | SslMode::Require => {
            builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider()))).with_no_client_auth()
        }
    };
    // reqwest leaves a preconfigured config alone, so offer HTTP/2 ourselves.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

fn transport_error(e: reqwest::Error) -> Error {
    Error::ConnectionFailed(error_chain(&e))
}

async fn check_status(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    // sqld and Turso answer errors with `{"error": "…"}` (or `{"message": …}`), sometimes plain text.
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| json.get("error").or(json.get("message")).and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_else(|| body.trim().chars().take(300).collect());
    let detail = if detail.is_empty() { status.canonical_reason().unwrap_or("").to_string() } else { detail };
    Err(match status.as_u16() {
        401 | 403 => Error::ConnectionFailed(format!("The server rejected the auth token (HTTP {}): {detail}", status.as_u16())),
        404 => Error::ConnectionFailed(format!(
            "Not found (HTTP 404): {detail}. Check the URL; the server must be a libSQL server (sqld 0.21 or later, or Turso)."
        )),
        code if code >= 500 => Error::ConnectionFailed(format!("Server error (HTTP {code}): {detail}")),
        code => Error::Query(format!("ERROR: {detail} (HTTP {code})")),
    })
}

/// A Hrana stream: requests on it share a SQLite connection (and so transactions). Requests must
/// be sent one after the other; always end with [`Stream::close`] (or a `Close` request).
pub(crate) struct Stream {
    client: Arc<Client>,
    baton: Option<String>,
    base_url: Option<String>,
}

impl Stream {
    /// Runs `requests` in order and returns one result per request.
    pub async fn pipeline(&mut self, requests: &[StreamRequest]) -> Result<Vec<StreamResult>> {
        let body = serde_json::to_vec(&PipelineRequest { baton: self.baton.as_deref(), requests })
            .map_err(|e| Error::Internal(e.to_string()))?;
        let base = self.base_url.clone().unwrap_or_else(|| self.client.base_url.clone());
        let response = self.client.post(&base, "v3/pipeline", body).await?;
        let bytes = response.bytes().await.map_err(transport_error)?;
        let parsed: PipelineResponse = serde_json::from_slice(&bytes)
            .map_err(|e| Error::ConnectionFailed(format!("Unexpected response from the server: {e}")))?;
        self.update(parsed.baton, parsed.base_url);
        if parsed.results.len() != requests.len() {
            return Err(Error::Internal("the server returned a different number of results".into()));
        }
        Ok(parsed.results)
    }

    /// Starts a cursor over `batch`; read it with [`Cursor::next`]. The stream's baton is updated
    /// from the cursor's first line.
    pub async fn cursor(&mut self, batch: &Batch) -> Result<Cursor> {
        let body = serde_json::to_vec(&CursorRequest { baton: self.baton.as_deref(), batch })
            .map_err(|e| Error::Internal(e.to_string()))?;
        let base = self.base_url.clone().unwrap_or_else(|| self.client.base_url.clone());
        let response = self.client.post(&base, "v3/cursor", body).await?;
        let mut cursor = Cursor { response, lines: LineBuffer::default() };
        let header: CursorHeader = match cursor.line().await? {
            Some(line) => serde_json::from_slice(&line)
                .map_err(|e| Error::ConnectionFailed(format!("Unexpected response from the server: {e}")))?,
            None => return Err(Error::ConnectionFailed("The server closed the response early.".into())),
        };
        self.update(header.baton, header.base_url);
        Ok(cursor)
    }

    fn update(&mut self, baton: Option<String>, base_url: Option<String>) {
        self.baton = baton;
        if base_url.is_some() {
            self.base_url = base_url;
        }
    }

    /// Whether the server still holds the stream open.
    pub fn is_open(&self) -> bool {
        self.baton.is_some()
    }

    /// Closes the stream (rolling back an unfinished transaction). Errors are ignored: an expired
    /// stream is closed anyway.
    pub async fn close(mut self) {
        if self.is_open() {
            let _ = self.pipeline(&[StreamRequest::Close]).await;
        }
    }
}

/// Entries of a `v3/cursor` response, one JSON line each.
pub(crate) struct Cursor {
    response: reqwest::Response,
    lines: LineBuffer,
}

impl Cursor {
    /// The next entry, or `None` at the end of the batch.
    pub async fn next(&mut self) -> Result<Option<CursorEntry>> {
        loop {
            let Some(line) = self.line().await? else { return Ok(None) };
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|e| Error::ConnectionFailed(format!("Unexpected response from the server: {e}")));
        }
    }

    async fn line(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(line) = self.lines.pop() {
                return Ok(Some(line));
            }
            match self.response.chunk().await.map_err(transport_error)? {
                Some(chunk) => self.lines.push(&chunk),
                None => return Ok(self.lines.finish()),
            }
        }
    }
}

/// Splits a byte stream into `\n`-terminated lines, whatever the chunk boundaries.
#[derive(Default)]
pub(crate) struct LineBuffer {
    buffer: Vec<u8>,
    /// Bytes before this offset hold no newline (so a long line isn't rescanned per chunk).
    scanned: usize,
}

impl LineBuffer {
    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let newline = self.buffer[self.scanned..].iter().position(|&b| b == b'\n').map(|i| i + self.scanned);
        match newline {
            Some(end) => {
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                self.scanned = 0;
                Some(line)
            }
            None => {
                self.scanned = self.buffer.len();
                None
            }
        }
    }

    /// What's left after the last newline, if anything.
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        self.scanned = 0;
        (!self.buffer.is_empty()).then(|| std::mem::take(&mut self.buffer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_requests() {
        let batch = Batch::chained([Stmt::new("begin"), Stmt::with_args("select ?1", &["x"])]);
        let json = serde_json::to_value(&[StreamRequest::Batch { batch }, StreamRequest::Close]).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"type": "batch", "batch": {"steps": [
                    {"stmt": {"sql": "begin", "want_rows": true}},
                    {"condition": {"type": "ok", "step": 0}, "stmt": {"sql": "select ?1", "args": [{"type": "text", "value": "x"}], "want_rows": true}}
                ]}},
                {"type": "close"}
            ])
        );
    }

    #[test]
    fn parses_pipeline_responses() {
        let body = r#"{"baton":"b1","base_url":null,"results":[
            {"type":"ok","response":{"type":"execute","result":{"cols":[{"name":"id","decltype":"INTEGER"},{"name":"x","decltype":null}],
              "rows":[[{"type":"integer","value":"9007199254740993"},{"type":"blob","base64":"AQI"}],[{"type":"float","value":1.5},{"type":"null"}]],
              "affected_row_count":0,"last_insert_rowid":null,"rows_read":2,"rows_written":0,"query_duration_ms":0.1}}},
            {"type":"error","error":{"message":"SQLite error: no such table: nope","code":"SQLITE_UNKNOWN"}},
            {"type":"ok","response":{"type":"close"}}]}"#;
        let parsed: PipelineResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.baton.as_deref(), Some("b1"));
        let StreamResult::Ok { response: StreamResponse::Execute { result } } = &parsed.results[0] else { panic!() };
        assert_eq!(result.cols[0].decltype.as_deref(), Some("INTEGER"));
        assert_eq!(result.rows[0][0].decode("integer"), Value::Int(9007199254740993));
        assert_eq!(result.rows[0][1].decode(""), Value::Text("0x0102".into()));
        assert_eq!(result.rows[1][0].decode("numeric"), Value::Decimal("1.5".into()));
        let StreamResult::Error { error } = &parsed.results[1] else { panic!() };
        assert_eq!(error.message(), "ERROR: no such table: nope");
        assert!(matches!(parsed.results[2], StreamResult::Ok { response: StreamResponse::Other }));
    }

    #[test]
    fn parses_cursor_entries() {
        let lines = [
            r#"{"type":"step_begin","step":0,"cols":[{"name":"n","decltype":null}]}"#,
            r#"{"type":"row","row":[{"type":"text","value":"a"}]}"#,
            r#"{"type":"step_end","affected_row_count":0,"last_insert_rowid":null}"#,
            r#"{"type":"step_error","step":1,"error":{"message":"interrupted","code":"SQLITE_INTERRUPT"}}"#,
            r#"{"type":"replication_index","replication_index":"183"}"#,
        ];
        let entries: Vec<CursorEntry> = lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert!(matches!(&entries[0], CursorEntry::StepBegin { cols } if cols.len() == 1));
        assert!(matches!(&entries[1], CursorEntry::Row { row } if row[0].as_text().as_deref() == Some("a")));
        assert!(matches!(entries[4], CursorEntry::Other));
        assert!(matches!(&entries[3], CursorEntry::StepError { error, .. } if matches!(error.clone().into_error(), Error::Cancelled)));
    }

    #[test]
    fn round_trips_cursor_values() {
        for value in [
            CursorValue::Null,
            CursorValue::Int(i64::MIN),
            CursorValue::float(1.0 / 3.0),
            CursorValue::Text("é".into()),
            CursorValue::Bytes(vec![0, 255, 7]),
        ] {
            let json = serde_json::to_value(Arg::from_cursor(&value)).unwrap();
            let back: HValue = serde_json::from_value(json).unwrap();
            assert_eq!(back.to_cursor(), value);
        }
    }

    #[test]
    fn splits_lines_across_chunks() {
        let mut lines = LineBuffer::default();
        lines.push(b"{\"a\":");
        assert_eq!(lines.pop(), None);
        lines.push(b"1}\n{\"b\"");
        assert_eq!(lines.pop().as_deref(), Some(&b"{\"a\":1}"[..]));
        assert_eq!(lines.pop(), None);
        lines.push(b":2}\n\n{\"c\":3}");
        assert_eq!(lines.pop().as_deref(), Some(&b"{\"b\":2}"[..]));
        assert_eq!(lines.pop().as_deref(), Some(&b""[..]));
        assert_eq!(lines.pop(), None);
        assert_eq!(lines.finish().as_deref(), Some(&b"{\"c\":3}"[..]));
        assert_eq!(lines.finish(), None);
    }
}
