//! Tries "Sign in with Cloudflare" (`dbcore::d1::oauth`) from the command line:
//!   cargo run -p dbcore --example d1_oauth -- [--url d1://<account>/<database>]
//!
//! Opens the browser at Cloudflare's consent page, waits for the redirect and gets the sign-in's
//! secret (what the apps keep in the keychain). Then lists the accounts and their D1 databases,
//! refreshes the access token once, and (with `--url`) connects with the secret and runs `select 1`.
//! Tokens aren't printed or saved.
//!
//! Options: `--client-id` (default: dbear's; or `DBEAR_CF_OAUTH_CLIENT_ID`), `--scope <s>` (repeat;
//! default `d1.write account-settings.read offline_access`), `--redirect <uri>` (default
//! `http://127.0.0.1:38257/oauth/callback`; must match the client's registered redirect),
//! `--no-browser` (print the URL only).

use dbcore::d1::oauth::{self, OAuthClient, SignIn, LOGIN_TIMEOUT};
use dbcore::{Connection, ConnectionConfig};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut client = OAuthClient::new(oauth::client_id());
    let mut scopes = Vec::new();
    let mut url = None;
    let mut browser = true;
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage(&format!("{arg} needs a value")));
        match arg.as_str() {
            "--client-id" => client.client_id = value(),
            "--scope" => scopes.push(value()),
            "--redirect" => client.redirect_uri = value(),
            "--url" => url = Some(value()),
            "--no-browser" => browser = false,
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }
    if !scopes.is_empty() {
        client.scopes = scopes;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let result = runtime.block_on(async {
        let sign_in = SignIn::start_with(client, LOGIN_TIMEOUT)?;
        eprintln!("Sign in at:\n  {}\n", sign_in.url());
        if browser {
            open_browser(sign_in.url()).map_err(|e| dbcore::Error::ConnectionFailed(format!("Couldn't open the browser: {e}")))?;
        }
        let secret = sign_in.finish().await?;
        eprintln!("Signed in ({} byte secret).", secret.len());

        for account in dbcore::d1::accounts(secret.clone()).await? {
            let databases = dbcore::d1::databases(secret.clone(), account.id.clone()).await?;
            eprintln!("{} ({}): {}", account.name, account.id, databases.join(", "));
        }
        oauth::access_token("", &secret, true).await?;
        eprintln!("Refreshed the access token.");
        Ok::<_, dbcore::Error>(oauth::latest_secret(&secret))
    });
    let secret = result.unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });

    let Some(url) = url else { return };
    let mut config = ConnectionConfig::from_url(&url).unwrap_or_else(|e| usage(&e.to_string()));
    config.password = Some(secret);
    let connection = Connection::new(config);
    let check = runtime.block_on(async {
        connection.connect().await?;
        let result = connection.execute("select 1 as ok".into()).await?;
        eprintln!("select 1: {:?}", result.rows);
        Ok::<_, dbcore::Error>(())
    });
    if let Err(e) = check {
        eprintln!("D1 with the sign-in: {e}");
        std::process::exit(1);
    }
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
    eprintln!("usage: d1_oauth [--client-id <id>] [--scope <s>]... [--redirect <uri>] [--url d1://<account>/<database>] [--no-browser]");
    std::process::exit(2)
}
