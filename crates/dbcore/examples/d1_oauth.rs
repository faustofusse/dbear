//! Tries "Sign in with Cloudflare" (`dbcore::d1::oauth`) from the command line:
//!   cargo run -p dbcore --example d1_oauth -- --client-id <id> [--url d1:///<database>]
//!
//! Opens the browser at Cloudflare's consent page, waits for the redirect, trades the code for
//! tokens, refreshes once to check refresh tokens work, then (with `--url`) uses the access token as
//! the D1 API token to list the account's databases and run `select 1`. Tokens aren't printed or saved.
//!
//! Options: `--client-id` (or `DBEAR_CF_OAUTH_CLIENT_ID`), `--scope <s>` (repeat; default
//! `d1.write account-settings.read offline_access`), `--redirect <uri>` (default
//! `http://127.0.0.1:38257/oauth/callback`; must match the client's registered redirect),
//! `--no-browser` (print the URL only).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbcore::d1::oauth::{OAuthClient, LOGIN_TIMEOUT};
use dbcore::{Connection, ConnectionConfig};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut client_id = std::env::var("DBEAR_CF_OAUTH_CLIENT_ID").unwrap_or_default();
    let mut scopes = Vec::new();
    let mut redirect = None;
    let mut url = None;
    let mut browser = true;
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage(&format!("{arg} needs a value")));
        match arg.as_str() {
            "--client-id" => client_id = value(),
            "--scope" => scopes.push(value()),
            "--redirect" => redirect = Some(value()),
            "--url" => url = Some(value()),
            "--no-browser" => browser = false,
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }
    if client_id.is_empty() {
        usage("--client-id (or DBEAR_CF_OAUTH_CLIENT_ID) is required");
    }
    let mut client = OAuthClient::new(client_id);
    if !scopes.is_empty() {
        client.scopes = scopes;
    }
    if let Some(redirect) = redirect {
        client.redirect_uri = redirect;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let result = runtime.block_on(async {
        let open = |url: &str| {
            eprintln!("Sign in at:\n  {url}\n");
            if browser {
                open_browser(url)
            } else {
                Ok(())
            }
        };
        eprintln!("Waiting for Cloudflare on {} …", client.redirect_uri);
        let tokens = client.login(open, LOGIN_TIMEOUT).await?;
        describe("Signed in", &tokens);

        let Some(refresh_token) = tokens.refresh_token.clone() else {
            eprintln!("No refresh token: ask for the offline_access scope.");
            return Ok(tokens.access_token);
        };
        let refreshed = client.refresh(&refresh_token).await?;
        describe("Refreshed", &refreshed);
        if refreshed.refresh_token.as_deref() != Some(refresh_token.as_str()) {
            eprintln!("  (the refresh token rotated)");
        }
        Ok::<_, dbcore::Error>(refreshed.access_token)
    });
    let access_token = result.unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });

    let Some(url) = url else { return };
    let mut config = ConnectionConfig::from_url(&url).unwrap_or_else(|e| usage(&e.to_string()));
    config.password = Some(access_token);
    let connection = Connection::new(config);
    let check = runtime.block_on(async {
        connection.connect().await?;
        let databases = connection.list_databases().await?;
        eprintln!("Databases: {}", databases.join(", "));
        let result = connection.execute("select 1 as ok".into()).await?;
        eprintln!("select 1: {:?}", result.rows);
        Ok::<_, dbcore::Error>(())
    });
    if let Err(e) = check {
        eprintln!("D1 with the access token: {e}");
        std::process::exit(1);
    }
}

fn describe(what: &str, tokens: &dbcore::d1::oauth::Tokens) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs();
    eprintln!(
        "{what}: access token {}… expires in {} min, refresh token: {}, scopes: {}",
        tokens.access_token.chars().take(6).collect::<String>(),
        tokens.expires_at.saturating_sub(now) / 60,
        if tokens.refresh_token.is_some() { "yes" } else { "no" },
        if tokens.scopes.is_empty() { "(not listed)".into() } else { tokens.scopes.join(" ") },
    );
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(windows)]
    let mut command = {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(url).spawn().map(|_| ())
}

fn usage(error: &str) -> ! {
    if !error.is_empty() {
        eprintln!("{error}\n");
    }
    eprintln!("usage: d1_oauth --client-id <id> [--scope <s>]... [--redirect <uri>] [--url d1://<account>/<database>] [--no-browser]");
    std::process::exit(2)
}
