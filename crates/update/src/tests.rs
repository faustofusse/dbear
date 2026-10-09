use std::io::{BufRead as _, BufReader};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;

use super::*;

fn manifest(version: &str, artifacts: Vec<Artifact>) -> Manifest {
    Manifest {
        product: PRODUCT.into(),
        version: version.into(),
        pub_date: Some("2026-01-01T00:00:00Z".into()),
        notes: Some("Fixes".into()),
        notes_url: None,
        artifacts,
    }
}

fn artifact_for(path: &Path, url: &str, key: &SecretKey) -> Artifact {
    let (size, sha256, signature) = sign_file(path, key).unwrap();
    Artifact { target: "windows-x86_64".into(), kind: "nsis".into(), url: url.into(), size, sha256, signature }
}

#[test]
fn keys_round_trip() {
    let key = SecretKey::generate().unwrap();
    let again: SecretKey = key.to_base64().parse().unwrap();
    assert_eq!(again.public_key(), key.public_key());
    let public: PublicKey = key.public_key().to_string().parse().unwrap();
    assert_eq!(public, key.public_key());
    assert!("abc".parse::<PublicKey>().is_err());
    let other = SecretKey::generate().unwrap();
    let list = format!("{}, {}\n", key.public_key(), other.public_key());
    assert_eq!(parse_public_keys(&list).unwrap().len(), 2);
    assert!(!format!("{key:?}").contains(&key.to_base64()));
}

#[test]
fn manifest_signature_is_checked() {
    let key = SecretKey::generate().unwrap();
    let keys = [key.public_key()];
    let signed = SignedManifest::sign(&manifest("1.2.0", vec![]), &key);
    let json = signed.to_json();
    assert_eq!(verify_manifest(json.as_bytes(), &keys).unwrap().version, "1.2.0");

    // Another key.
    let other = SecretKey::generate().unwrap();
    assert!(matches!(verify_manifest(json.as_bytes(), &[other.public_key()]), Err(Error::BadSignature)));
    // Either of two keys (rotation).
    assert!(verify_manifest(json.as_bytes(), &[other.public_key(), key.public_key()]).is_ok());
    // No keys: never trusted.
    assert!(verify_manifest(json.as_bytes(), &[]).is_err());

    // A changed payload.
    let mut tampered = signed.clone();
    tampered.payload = tampered.payload.replace("1.2.0", "9.9.9");
    assert!(matches!(verify_manifest(tampered.to_json().as_bytes(), &keys), Err(Error::BadSignature)));
    // A signature of the payload without the context.
    let mut raw = signed.clone();
    raw.signature = B64.encode(key.0.sign(signed.payload.as_bytes()).to_bytes());
    assert!(matches!(verify_manifest(raw.to_json().as_bytes(), &keys), Err(Error::BadSignature)));

    // Another product, validly signed.
    let mut foreign = manifest("1.0.0", vec![]);
    foreign.product = "other".into();
    assert!(matches!(verify_manifest(SignedManifest::sign(&foreign, &key).to_json().as_bytes(), &keys), Err(Error::BadManifest(_))));
    // Garbage.
    assert!(matches!(verify_manifest(b"<html>", &keys), Err(Error::BadManifest(_))));
}

#[test]
fn files_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let key = SecretKey::generate().unwrap();
    let keys = [key.public_key()];
    let path = dir.path().join("setup.exe");
    std::fs::write(&path, b"installer bytes").unwrap();
    let artifact = artifact_for(&path, "https://example.com/setup.exe", &key);
    verify_file(&path, &artifact, &keys).unwrap();

    std::fs::write(&path, b"installer bytez").unwrap();
    assert!(matches!(verify_file(&path, &artifact, &keys), Err(Error::Mismatch(_))));

    // Right hash and size, signature by another key.
    std::fs::write(&path, b"installer bytes").unwrap();
    let other = SecretKey::generate().unwrap();
    let forged = Artifact { signature: artifact_for(&path, "", &other).signature, ..artifact.clone() };
    assert!(matches!(verify_file(&path, &forged, &keys), Err(Error::BadSignature)));
}

#[test]
fn versions() {
    assert!(is_newer("0.2.0", "0.1.9"));
    assert!(is_newer("v1.0.0", "0.9.0"));
    assert!(is_newer("1.0.0", "1.0.0-beta.2"));
    assert!(!is_newer("1.0.0", "1.0.0"));
    assert!(!is_newer("0.1.0", "0.2.0"));
    assert!(!is_newer("garbage", "0.1.0"));
    assert!(current_target().contains('-'));
}

#[test]
fn file_names_stay_in_the_folder() {
    assert_eq!(file_name("https://x/a/dbear-1.0.0-setup.exe?x=1", "1.0.0"), "dbear-1.0.0-setup.exe");
    assert_eq!(file_name("https://x/..%2f..%2fevil.exe", "1"), "2f..2fevil.exe");
    assert_eq!(file_name("https://x/", "1.0.0"), "dbear-1.0.0-update");
    assert_eq!(file_name("https://x/..", "1.0.0"), "dbear-1.0.0-update");
}

#[test]
fn check_and_download_over_http() {
    let key = SecretKey::generate().unwrap();
    let keys = [key.public_key()];
    let dir = tempfile::tempdir().unwrap();
    let payload = b"MZ new installer".repeat(10_000);
    let src = dir.path().join("src.exe");
    std::fs::write(&src, &payload).unwrap();

    let listener_base = serve_two_phase(&key, &src, &payload);
    let feed = format!("{listener_base}/dbear-update-windows.json");

    assert!(check(&feed, &keys, "0.2.0", "windows-x86_64", "nsis").unwrap().is_none(), "same version");
    assert!(check(&feed, &keys, "0.1.0", "windows-aarch64", "nsis").unwrap().is_none(), "other target");
    let update = check(&feed, &keys, "0.1.0", "windows-x86_64", "nsis").unwrap().expect("an update");
    assert_eq!(update.version(), "0.2.0");

    let downloads = dir.path().join("updates");
    let mut last = (0, 0);
    let path = download(&update, &downloads, &keys, &AtomicBool::new(false), |d, t| last = (d, t)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), payload);
    assert_eq!(last, (payload.len() as u64, payload.len() as u64));
    assert_eq!(path.file_name().unwrap(), "dbear-0.2.0-setup.exe");
    // Already there and verified: not downloaded again.
    assert_eq!(download(&update, &downloads, &keys, &AtomicBool::new(true), |_, _| {}).unwrap(), path);

    // Served bytes that don't match: rejected, nothing left behind.
    let mut bad = update.clone();
    bad.artifact.url = format!("{listener_base}/tampered.exe");
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(download(&bad, &downloads, &keys, &AtomicBool::new(false), |_, _| {}), Err(Error::Mismatch(_))));
    assert_eq!(std::fs::read_dir(&downloads).unwrap().count(), 0);

    // Cancelled.
    assert!(matches!(download(&update, &downloads, &keys, &AtomicBool::new(true), |_, _| {}), Err(Error::Cancelled)));

    // Missing feed.
    assert!(matches!(check(&format!("{listener_base}/missing.json"), &keys, "0.1.0", "x", "nsis"), Err(Error::NoFeed(_))));
}

fn serve_two_phase(key: &SecretKey, src: &Path, payload: &[u8]) -> String {
    // Reserve a port, then serve on it with the manifest that names it.
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let base = format!("http://{addr}");
    let artifact = artifact_for(src, &format!("{base}/download/v0.2.0/dbear-0.2.0-setup.exe"), key);
    let signed = SignedManifest::sign(&manifest("0.2.0", vec![artifact]), key).to_json().into_bytes();
    let mut tampered = payload.to_vec();
    tampered[0] ^= 1;
    let listener = TcpListener::bind(addr).unwrap();
    let routes: Vec<(&'static str, Vec<u8>)> = vec![
        ("/dbear-update-windows.json", signed),
        ("/download/v0.2.0/dbear-0.2.0-setup.exe", payload.to_vec()),
        ("/tampered.exe", tampered),
    ];
    serve_on(listener, routes);
    base
}

/// Serves `routes` (path → body) over HTTP until the test ends.
fn serve_on(listener: TcpListener, routes: Vec<(&'static str, Vec<u8>)>) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap() <= 2 {
                    break;
                }
            }
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            match routes.iter().find(|(p, _)| *p == path) {
                Some((_, body)) => {
                    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(body);
                }
                None => {
                    let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
            }
        }
    });
}

#[test]
fn file_feed() {
    let key = SecretKey::generate().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let feed = dir.path().join("feed.json");
    std::fs::write(&feed, SignedManifest::sign(&manifest("3.0.0", vec![]), &key).to_json()).unwrap();
    let url = format!("file://{}", feed.display());
    assert_eq!(fetch_manifest(&url, &[key.public_key()]).unwrap().version, "3.0.0");
    assert!(matches!(fetch_manifest(&format!("{url}.missing"), &[key.public_key()]), Err(Error::NoFeed(_))));
}
