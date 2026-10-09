//! Release tooling for dbear's self-updater.
//!
//! ```text
//! dbear-update keygen
//!     Prints a new private key (keep it secret: DBEAR_UPDATE_PRIVATE_KEY) and its public key.
//! dbear-update public-key
//!     The public key of DBEAR_UPDATE_PRIVATE_KEY.
//! dbear-update sign --version 1.2.0 [--notes FILE] [--notes-url URL] [--pub-date RFC3339]
//!                   --artifact TARGET KIND FILE URL [--artifact …] --out dbear-update-windows.json
//!     Signs each artifact and the manifest with DBEAR_UPDATE_PRIVATE_KEY.
//! dbear-update verify MANIFEST [--key PUBLIC_KEYS] [--file TARGET KIND FILE …]
//!     Checks a manifest (and files against it) with the given public keys (default:
//!     DBEAR_UPDATE_PUBLIC_KEY). Prints the manifest.
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use dbear_update::{Artifact, Manifest, PRODUCT, SecretKey, SignedManifest};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dbear-update: {e}");
            ExitCode::FAILURE
        }
    }
}

fn private_key() -> Result<SecretKey, String> {
    let value = std::env::var("DBEAR_UPDATE_PRIVATE_KEY").map_err(|_| "DBEAR_UPDATE_PRIVATE_KEY isn’t set".to_string())?;
    value.parse().map_err(|e| format!("DBEAR_UPDATE_PRIVATE_KEY: {e}"))
}

fn run(args: &[String]) -> Result<(), String> {
    let Some((command, rest)) = args.split_first() else {
        return Err(usage());
    };
    match command.as_str() {
        "keygen" => {
            let key = SecretKey::generate().map_err(|e| e.to_string())?;
            println!("private key (secret DBEAR_UPDATE_PRIVATE_KEY): {}", key.to_base64());
            println!("public key  (packaging/update-public-key): {}", key.public_key());
            Ok(())
        }
        "public-key" => {
            println!("{}", private_key()?.public_key());
            Ok(())
        }
        "sign" => sign(rest),
        "verify" => verify(rest),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage: dbear-update keygen | public-key | sign … | verify … (see the source’s header)".into()
}

fn take<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, String> {
    it.next().cloned().ok_or_else(|| format!("{flag} needs a value"))
}

fn sign(args: &[String]) -> Result<(), String> {
    let key = private_key()?;
    let mut version = None;
    let mut notes = None;
    let mut notes_url = None;
    let mut pub_date = None;
    let mut out = None;
    let mut artifacts = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--version" => version = Some(take(&mut it, arg)?),
            "--notes" => {
                let path = take(&mut it, arg)?;
                notes = Some(std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?);
            }
            "--notes-url" => notes_url = Some(take(&mut it, arg)?),
            "--pub-date" => pub_date = Some(take(&mut it, arg)?),
            "--out" => out = Some(PathBuf::from(take(&mut it, arg)?)),
            "--artifact" => {
                let (target, kind, file, url) = (take(&mut it, arg)?, take(&mut it, arg)?, take(&mut it, arg)?, take(&mut it, arg)?);
                let (size, sha256, signature) =
                    dbear_update::sign_file(file.as_ref(), &key).map_err(|e| format!("{file}: {e}"))?;
                artifacts.push(Artifact { target, kind, url, size, sha256, signature });
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    let version = version.ok_or("--version is required")?;
    let version = version.trim_start_matches('v').to_string();
    dbear_update::parse_version(&version).map_err(|e| e.to_string())?;
    if artifacts.is_empty() {
        return Err("at least one --artifact is required".into());
    }
    let manifest = Manifest { product: PRODUCT.into(), version, pub_date, notes: notes.filter(|n| !n.trim().is_empty()), notes_url, artifacts };
    let json = SignedManifest::sign(&manifest, &key).to_json();
    // Check what was written with the public half, as an app would.
    dbear_update::verify_manifest(json.as_bytes(), &[key.public_key()]).map_err(|e| e.to_string())?;
    match out {
        Some(path) => std::fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display())),
        None => {
            print!("{json}");
            Ok(())
        }
    }
}

fn verify(args: &[String]) -> Result<(), String> {
    let mut it = args.iter();
    let path = take(&mut it, "verify")?;
    let mut keys = std::env::var("DBEAR_UPDATE_PUBLIC_KEY").unwrap_or_default();
    let mut files = Vec::new();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--key" => keys = take(&mut it, arg)?,
            "--file" => files.push((take(&mut it, arg)?, take(&mut it, arg)?, take(&mut it, arg)?)),
            other => return Err(format!("unknown option {other}")),
        }
    }
    let keys = dbear_update::parse_public_keys(&keys).map_err(|e| e.to_string())?;
    let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
    let manifest = dbear_update::verify_manifest(&bytes, &keys).map_err(|e| format!("{path}: {e}"))?;
    for (target, kind, file) in files {
        let artifact = manifest.artifact(&target, &kind).ok_or_else(|| format!("no {kind} artifact for {target}"))?;
        dbear_update::verify_file(file.as_ref(), artifact, &keys).map_err(|e| format!("{file}: {e}"))?;
        eprintln!("ok: {file} ({target}, {kind})");
    }
    println!("{}", serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?);
    Ok(())
}
