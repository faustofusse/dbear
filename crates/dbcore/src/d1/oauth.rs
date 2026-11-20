//! Sign in with Cloudflare: OAuth 2.0 Authorization Code with PKCE, as a public client (no client
//! secret, so nothing secret ships in the app).
//!
//! The app registers an OAuth client in a Cloudflare account (dashboard ▸ Manage Account ▸ OAuth
//! clients: grant `authorization_code`, token auth method `none`, a loopback redirect URL). Then:
//! 1. [`OAuthClient::login`] listens on the redirect's loopback address, opens the browser at
//!    Cloudflare's consent page and waits for the redirect with the authorization code.
//! 2. It trades the code (and the PKCE verifier) for [`Tokens`]: a short-lived access token, used
//!    as the D1 API token, and a refresh token.
//! 3. [`OAuthClient::refresh`] gets a new access token later. Refresh tokens may rotate: keep the
//!    one each refresh returns.
//!
//! Endpoints: `https://dash.cloudflare.com/oauth2/{auth,token,revoke}`. `DBEAR_CF_OAUTH` points
//! them at another server (tests).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::dialect::error_chain;
use crate::driver::{Error, Result};
use crate::model::SslMode;

/// What dbear asks for: D1 (read and write), the user's accounts (to pick one), and a refresh token.
/// Scope names follow Cloudflare's API token permissions (`GET /client/v4/oauth/scopes`).
pub const DEFAULT_SCOPES: &[&str] = &["d1.write", "account-settings.read", "offline_access"];

/// The loopback redirect. Registered clients must list it exactly, so the port is fixed.
pub const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:38257/oauth/callback";

/// How long [`OAuthClient::login`] waits for the browser to come back.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn oauth_base() -> String {
    std::env::var("DBEAR_CF_OAUTH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://dash.cloudflare.com/oauth2".into())
}

/// A registered public OAuth client.
#[derive(Debug, Clone)]
pub struct OAuthClient {
    pub client_id: String,
    pub scopes: Vec<String>,
    /// A loopback URL (`http://127.0.0.1:<port>/<path>`). Port 0 takes any free port (only works if
    /// Cloudflare accepts any loopback port for the client).
    pub redirect_uri: String,
    /// Where `auth`, `token` and `revoke` live.
    pub base: String,
}

/// The tokens a login or refresh returns. `expires_at` is in seconds since the Unix epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
    pub scopes: Vec<String>,
}

impl Tokens {
    /// True when the access token expires within `margin` (or already has).
    pub fn expires_within(&self, margin: Duration) -> bool {
        now() + margin.as_secs() >= self.expires_at
    }
}

/// A PKCE verifier and its S256 challenge (RFC 7636).
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Result<Self> {
        Ok(Self::from_verifier(random_token(32)?))
    }

    pub fn from_verifier(verifier: String) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
        Self { challenge: URL_SAFE_NO_PAD.encode(digest.as_ref()), verifier }
    }
}

/// One login attempt: the consent page's URL and what the code exchange needs.
#[derive(Debug, Clone)]
pub struct Authorization {
    pub url: String,
    pub redirect_uri: String,
    pub state: String,
    pkce: Pkce,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

impl OAuthClient {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            scopes: DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
            redirect_uri: DEFAULT_REDIRECT_URI.into(),
            base: oauth_base(),
        }
    }

    /// Signs in: listens for the redirect, calls `open` with the consent page's URL (open it in the
    /// browser) and waits up to `timeout` for the user to come back.
    pub async fn login(&self, open: impl FnOnce(&str) -> std::io::Result<()>, timeout: Duration) -> Result<Tokens> {
        let (listener, redirect_uri) = self.listen().await?;
        let authorization = self.authorize(&redirect_uri)?;
        open(&authorization.url).map_err(|e| Error::ConnectionFailed(format!("Couldn't open the browser: {e}")))?;
        let code = tokio::time::timeout(timeout, wait_for_code(&listener, &authorization.state))
            .await
            .map_err(|_| Error::ConnectionFailed("Cloudflare sign-in timed out.".into()))??;
        self.exchange(&authorization, &code).await
    }

    /// Binds the redirect's loopback address. Returns the redirect URI with the actual port.
    async fn listen(&self) -> Result<(TcpListener, String)> {
        let mut url = url::Url::parse(&self.redirect_uri).map_err(|e| Error::InvalidConfig(format!("Redirect URL: {e}")))?;
        let host = match url.host_str() {
            Some(h @ ("127.0.0.1" | "localhost")) => h.to_string(),
            Some("[::1]") => "::1".into(),
            _ => return Err(Error::InvalidConfig("The redirect URL must be a loopback address (http://127.0.0.1:<port>/…).".into())),
        };
        let port = url.port().unwrap_or(80);
        let bind = if host == "localhost" { "127.0.0.1".to_string() } else { host };
        let listener = TcpListener::bind((bind.as_str(), port)).await.map_err(|e| {
            Error::ConnectionFailed(format!("Couldn't listen on port {port} for Cloudflare's sign-in ({e}). Is another sign-in open?"))
        })?;
        let actual = listener.local_addr().map_err(|e| Error::Internal(e.to_string()))?.port();
        let _ = url.set_port(Some(actual));
        Ok((listener, url.to_string()))
    }

    /// The consent page's URL for a new PKCE pair and state.
    pub fn authorize(&self, redirect_uri: &str) -> Result<Authorization> {
        let pkce = Pkce::new()?;
        let state = random_token(16)?;
        let mut url = url::Url::parse(&format!("{}/auth", self.base)).map_err(|e| Error::InvalidConfig(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &self.scopes.join(" "))
            .append_pair("state", &state)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(Authorization { url: url.into(), redirect_uri: redirect_uri.into(), state, pkce })
    }

    /// Trades an authorization code for tokens.
    pub async fn exchange(&self, authorization: &Authorization, code: &str) -> Result<Tokens> {
        self.token_request(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &authorization.redirect_uri),
            ("client_id", &self.client_id),
            ("code_verifier", &authorization.pkce.verifier),
        ])
        .await
    }

    /// A new access token. Keeps `refresh_token` when the answer has no new one.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens> {
        let mut tokens =
            self.token_request(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", &self.client_id)]).await?;
        tokens.refresh_token.get_or_insert_with(|| refresh_token.to_string());
        Ok(tokens)
    }

    /// Revokes a token (refresh tokens end the whole grant). For signing out.
    pub async fn revoke(&self, token: &str) -> Result<()> {
        let response = self.post_form("revoke", &[("token", token), ("client_id", &self.client_id)]).await?;
        if response.0 >= 400 {
            return Err(oauth_error(response.0, &response.1));
        }
        Ok(())
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<Tokens> {
        let (status, body) = self.post_form("token", form).await?;
        if status >= 400 {
            return Err(oauth_error(status, &body));
        }
        let answer: TokenResponse = serde_json::from_slice(&body)
            .map_err(|e| Error::ConnectionFailed(format!("Unexpected answer from Cloudflare's sign-in ({e}).")))?;
        let scopes = answer.scope.map(|s| s.split_whitespace().map(str::to_string).collect()).unwrap_or_default();
        Ok(Tokens {
            access_token: answer.access_token,
            refresh_token: answer.refresh_token.filter(|t| !t.is_empty()),
            expires_at: now() + answer.expires_in.unwrap_or(3600),
            scopes,
        })
    }

    async fn post_form(&self, endpoint: &str, form: &[(&str, &str)]) -> Result<(u16, Vec<u8>)> {
        let body: String = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent("dbear")
            .use_preconfigured_tls(crate::libsql::hrana::tls_config(SslMode::VerifyFull)?)
            .build()
            .map_err(|e| Error::Internal(format!("HTTP client: {}", error_chain(&e))))?;
        let response = http
            .post(format!("{}/{endpoint}", self.base))
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| Error::ConnectionFailed(error_chain(&e)))?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(|e| Error::ConnectionFailed(error_chain(&e)))?;
        Ok((status, body.to_vec()))
    }
}

fn oauth_error(status: u16, body: &[u8]) -> Error {
    match serde_json::from_slice::<ErrorResponse>(body) {
        Ok(e) if e.error == "invalid_grant" => {
            Error::ConnectionFailed("The Cloudflare sign-in expired or was revoked. Sign in again.".into())
        }
        Ok(e) => Error::ConnectionFailed(match e.error_description {
            Some(d) if !d.is_empty() => format!("Cloudflare sign-in failed: {d} ({})", e.error),
            _ => format!("Cloudflare sign-in failed: {}", e.error),
        }),
        Err(_) => {
            let text: String = String::from_utf8_lossy(body).trim().chars().take(200).collect();
            Error::ConnectionFailed(format!("Cloudflare sign-in failed (HTTP {status}): {text}"))
        }
    }
}

/// What the browser brought back to the redirect.
#[derive(Debug, PartialEq, Eq)]
enum Callback {
    Code(String),
    Denied(String),
    /// Another path (`/favicon.ico`) or a request without the expected state.
    Ignore,
}

/// Parses a request line's target (`/oauth/callback?code=…&state=…`).
fn parse_callback(target: &str, state: &str) -> Callback {
    let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else { return Callback::Ignore };
    let mut code = None;
    let mut got_state = None;
    let mut error = None;
    let mut description = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => got_state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }
    if got_state.as_deref() != Some(state) {
        return Callback::Ignore;
    }
    match (code, error) {
        (_, Some(error)) => Callback::Denied(description.filter(|d| !d.is_empty()).unwrap_or(error)),
        (Some(code), None) if !code.is_empty() => Callback::Code(code),
        _ => Callback::Ignore,
    }
}

/// Answers requests on the redirect until one brings the code (or an error) for `state`.
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else { continue };
        let Some(target) = read_target(&mut stream).await else { continue };
        match parse_callback(&target, state) {
            Callback::Code(code) => {
                respond(&mut stream, 200, "Signed in to Cloudflare", "You can close this tab and go back to dbear.").await;
                return Ok(code);
            }
            Callback::Denied(reason) => {
                respond(&mut stream, 200, "Sign-in cancelled", &format!("Cloudflare said: {reason}. You can close this tab.")).await;
                return Err(Error::ConnectionFailed(format!("Cloudflare sign-in was cancelled: {reason}")));
            }
            Callback::Ignore => respond(&mut stream, 404, "Not found", "").await,
        }
    }
}

/// The request line's target, from a request head of at most 16 KB.
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await.ok()?.ok()?;
        if n == 0 || head.len() > 16 * 1024 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line = String::from_utf8_lossy(&head).lines().next()?.to_string();
    let mut parts = line.split(' ');
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => Some(target.to_string()),
        _ => None,
    }
}

async fn respond(stream: &mut TcpStream, status: u16, title: &str, message: &str) {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
         <body style=\"font:16px -apple-system,system-ui,sans-serif;text-align:center;margin-top:20vh\">\
         <h2>{title}</h2><p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn random_token(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| Error::Internal(format!("Random bytes: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256() {
        // base64url(sha256(verifier)) without padding, as Python's hashlib computes it.
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mJ92K9KK6ZaIr-r7hD1CJ8lUbbMuvM".into());
        assert_eq!(pkce.challenge, "srNE5BOQe0XqRp_0VL9LYsiJbFWEfbjTemMDjs2_UMI");
        let fresh = Pkce::new().unwrap();
        assert_eq!(fresh.verifier.len(), 43);
        assert_ne!(fresh.verifier, Pkce::new().unwrap().verifier);
    }

    #[test]
    fn authorize_url_has_pkce_and_scopes() {
        let mut client = OAuthClient::new("client-1");
        client.base = "https://dash.cloudflare.com/oauth2".into();
        let auth = client.authorize("http://127.0.0.1:38257/oauth/callback").unwrap();
        let url = url::Url::parse(&auth.url).unwrap();
        assert_eq!(url.path(), "/oauth2/auth");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "client-1");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:38257/oauth/callback");
        assert_eq!(q["scope"], "d1.write account-settings.read offline_access");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], auth.pkce.challenge);
        assert_eq!(q["state"], auth.state);
    }

    #[test]
    fn callbacks() {
        assert_eq!(parse_callback("/oauth/callback?code=abc&state=s1", "s1"), Callback::Code("abc".into()));
        assert_eq!(parse_callback("/oauth/callback?code=abc&state=other", "s1"), Callback::Ignore);
        assert_eq!(parse_callback("/favicon.ico", "s1"), Callback::Ignore);
        assert_eq!(
            parse_callback("/oauth/callback?error=access_denied&error_description=The+user+said+no&state=s1", "s1"),
            Callback::Denied("The user said no".into())
        );
        assert_eq!(parse_callback("/oauth/callback?error=access_denied&state=s1", "s1"), Callback::Denied("access_denied".into()));
    }

    #[test]
    fn expiry() {
        let tokens = Tokens { access_token: "a".into(), refresh_token: None, expires_at: now() + 120, scopes: vec![] };
        assert!(!tokens.expires_within(Duration::from_secs(60)));
        assert!(tokens.expires_within(Duration::from_secs(300)));
    }

    /// A token endpoint that answers one request with `answer` and hands back the form it got.
    async fn token_server(answer: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/oauth2", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some(split) = text.find("\r\n\r\n") {
                    let length: usize = text
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                        .unwrap_or(0);
                    if request.len() >= split + 4 + length || n == 0 {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                            answer.len()
                        );
                        stream.write_all(response.as_bytes()).await.unwrap();
                        return text[split + 4..].to_string();
                    }
                }
            }
        });
        (base, task)
    }

    /// A plain HTTP GET (the browser coming back to the redirect).
    async fn get(url: &str) -> String {
        let url = url::Url::parse(url).unwrap();
        let mut stream = TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())).await.unwrap();
        let target = format!("{}?{}", url.path(), url.query().unwrap_or_default());
        stream.write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn login_round_trip() {
        let (base, server) =
            token_server(r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600,"scope":"d1.write offline_access","token_type":"bearer"}"#)
                .await;
        let client = OAuthClient {
            client_id: "client-1".into(),
            scopes: vec!["d1.write".into()],
            redirect_uri: "http://127.0.0.1:0/oauth/callback".into(),
            base,
        };
        // The "browser": approves at once and follows the redirect with the state it was given.
        let open = |url: &str| {
            let url = url::Url::parse(url).unwrap();
            let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            let back = format!("{}?code=code-1&state={}", q["redirect_uri"], q["state"]);
            tokio::spawn(async move {
                let favicon = back.replace("/oauth/callback?", "/favicon.ico?");
                assert!(get(&favicon).await.starts_with("HTTP/1.1 404"));
                assert!(get(&back).await.contains("Signed in"));
            });
            Ok(())
        };
        let tokens = client.login(open, Duration::from_secs(10)).await.unwrap();
        assert_eq!(tokens.access_token, "at-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(tokens.scopes, ["d1.write", "offline_access"]);
        assert!(!tokens.expires_within(Duration::from_secs(3000)));

        let form: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(server.await.unwrap().as_bytes()).into_owned().collect();
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "code-1");
        assert_eq!(form["client_id"], "client-1");
        assert!(form["redirect_uri"].starts_with("http://127.0.0.1:"));
        assert!(!form["redirect_uri"].contains(":0/"));
        assert_eq!(form["code_verifier"].len(), 43);
    }

    #[tokio::test]
    async fn refresh_keeps_the_old_refresh_token_when_none_comes_back() {
        let (base, server) = token_server(r#"{"access_token":"at-2","expires_in":60}"#).await;
        let client = OAuthClient { base, ..OAuthClient::new("client-1") };
        let tokens = client.refresh("rt-1").await.unwrap();
        assert_eq!(tokens.access_token, "at-2");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
        let form = server.await.unwrap();
        assert!(form.contains("grant_type=refresh_token") && form.contains("refresh_token=rt-1"));
    }

    #[test]
    fn errors_read_well() {
        let e = oauth_error(400, br#"{"error":"invalid_grant","error_description":"expired"}"#);
        assert!(e.to_string().contains("Sign in again"), "{e}");
        let e = oauth_error(400, br#"{"error":"invalid_client","error_description":"Unknown client"}"#);
        assert!(e.to_string().contains("Unknown client"), "{e}");
        let e = oauth_error(502, b"<html>bad gateway</html>");
        assert!(e.to_string().contains("HTTP 502"), "{e}");
    }
}
