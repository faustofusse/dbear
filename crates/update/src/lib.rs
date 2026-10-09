//! dbear's self-updater core, shared by the frontends that update themselves (the GPUI app on
//! Windows today; Linux can plug in later). It has no UI and knows nothing about installers.
//!
//! A release publishes one **signed manifest** per platform family (`dbear-update-windows.json`)
//! as a GitHub release asset. The app reads it from `releases/latest/download/…`:
//!
//! ```json
//! { "payload": "<manifest JSON, as a string>", "signature": "<base64 ed25519>" }
//! ```
//!
//! The signature covers [`MANIFEST_CONTEXT`] followed by the payload's bytes, so the payload is
//! verified exactly as signed and parsed only afterwards. The [`Manifest`] lists the artifacts per
//! target (`windows-x86_64`, …) with their size, SHA-256 and an ed25519 signature of their own
//! (over [`ARTIFACT_CONTEXT`] + the SHA-256 digest). A download is used only when the size, the
//! hash and that signature all match; nothing unsigned is ever installed. HTTPS is not relied on:
//! a test feed can be served over plain HTTP.
//!
//! Keys are raw ed25519 keys in base64 (32 bytes): the private one is a secret of the release
//! machine or CI, the public one is compiled into the app.

use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signer as _, Verifier as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Signed together with a manifest's payload, so a signature can't be reused for anything else.
pub const MANIFEST_CONTEXT: &[u8] = b"dbear update manifest v1\n";
/// Signed together with an artifact's SHA-256.
pub const ARTIFACT_CONTEXT: &[u8] = b"dbear update artifact v1\n";
/// The manifests' `product`.
pub const PRODUCT: &str = "dbear";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Network(String),
    /// The feed isn't there (yet): a release without this platform's manifest.
    #[error("no update information at {0}")]
    NoFeed(String),
    #[error("the update isn’t signed with dbear’s key")]
    BadSignature,
    #[error("invalid update information: {0}")]
    BadManifest(String),
    #[error("the download doesn’t match the signed update: {0}")]
    Mismatch(String),
    #[error("invalid key: {0}")]
    BadKey(String),
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

// MARK: Keys

/// An ed25519 public key (base64 of its 32 bytes).
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey(ed25519_dalek::VerifyingKey);

impl FromStr for PublicKey {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        let bytes: [u8; 32] = decode32(s)?;
        ed25519_dalek::VerifyingKey::from_bytes(&bytes).map(PublicKey).map_err(|e| Error::BadKey(e.to_string()))
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&B64.encode(self.0.as_bytes()))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({self})")
    }
}

/// The public keys in `list` (separated by commas or whitespace), so a key can be rotated: an app
/// accepts both while the releases switch over.
pub fn parse_public_keys(list: &str) -> Result<Vec<PublicKey>> {
    list.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).map(str::parse).collect()
}

/// An ed25519 private key (base64 of its 32-byte seed). Never printed by `Debug`.
pub struct SecretKey(ed25519_dalek::SigningKey);

impl SecretKey {
    pub fn generate() -> Result<Self> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| Error::BadKey(e.to_string()))?;
        Ok(SecretKey(ed25519_dalek::SigningKey::from_bytes(&seed)))
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    /// The seed in base64, the form kept in secrets.
    pub fn to_base64(&self) -> String {
        B64.encode(self.0.to_bytes())
    }
}

impl FromStr for SecretKey {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(SecretKey(ed25519_dalek::SigningKey::from_bytes(&decode32(s)?)))
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretKey(public: {})", self.public_key())
    }
}

fn decode32(s: &str) -> Result<[u8; 32]> {
    let bytes = B64.decode(s.trim()).map_err(|e| Error::BadKey(e.to_string()))?;
    bytes.try_into().map_err(|b: Vec<u8>| Error::BadKey(format!("expected 32 bytes, got {}", b.len())))
}

fn signature(s: &str) -> Result<ed25519_dalek::Signature> {
    let bytes: [u8; 64] = B64.decode(s.trim()).ok().and_then(|b| b.try_into().ok()).ok_or(Error::BadSignature)?;
    Ok(ed25519_dalek::Signature::from_bytes(&bytes))
}

fn verify_any(keys: &[PublicKey], message: &[u8], sig: &str) -> Result<()> {
    let sig = signature(sig)?;
    if keys.iter().any(|key| key.0.verify(message, &sig).is_ok()) { Ok(()) } else { Err(Error::BadSignature) }
}

// MARK: Manifest

/// What a release offers one platform family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Always [`PRODUCT`].
    pub product: String,
    /// SemVer, without the tag's `v`.
    pub version: String,
    /// RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pub_date: Option<String>,
    /// Release notes (Markdown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// The release's page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    pub artifacts: Vec<Artifact>,
}

/// One downloadable file of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// `<os>-<arch>` as in [`current_target`], e.g. `windows-x86_64`.
    pub target: String,
    /// How it installs: `nsis` (Windows installer, run with `/S /UPDATE`), `zip`, later `appimage`…
    pub kind: String,
    pub url: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
    /// base64 ed25519 over [`ARTIFACT_CONTEXT`] + the raw SHA-256 digest.
    pub signature: String,
}

impl Manifest {
    /// The artifact for `target` of the given `kind`.
    pub fn artifact(&self, target: &str, kind: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.target == target && a.kind == kind)
    }
}

/// The file published as a release asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedManifest {
    pub payload: String,
    pub signature: String,
}

impl SignedManifest {
    pub fn sign(manifest: &Manifest, key: &SecretKey) -> Self {
        let payload = serde_json::to_string_pretty(manifest).expect("a manifest serializes");
        let signature = B64.encode(key.0.sign(&[MANIFEST_CONTEXT, payload.as_bytes()].concat()).to_bytes());
        SignedManifest { payload, signature }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a signed manifest serializes") + "\n"
    }
}

/// Checks the signature of a published manifest (`bytes`), then parses what it signs.
pub fn verify_manifest(bytes: &[u8], keys: &[PublicKey]) -> Result<Manifest> {
    if keys.is_empty() {
        return Err(Error::BadKey("no public key".into()));
    }
    let signed: SignedManifest = serde_json::from_slice(bytes).map_err(|e| Error::BadManifest(e.to_string()))?;
    verify_any(keys, &[MANIFEST_CONTEXT, signed.payload.as_bytes()].concat(), &signed.signature)?;
    let manifest: Manifest = serde_json::from_str(&signed.payload).map_err(|e| Error::BadManifest(e.to_string()))?;
    if manifest.product != PRODUCT {
        return Err(Error::BadManifest(format!("it’s for “{}”", manifest.product)));
    }
    parse_version(&manifest.version)?;
    Ok(manifest)
}

/// Size, SHA-256 and signature of the file at `path`, for its [`Artifact`].
pub fn sign_file(path: &Path, key: &SecretKey) -> Result<(u64, String, String)> {
    let (size, digest) = hash_file(path)?;
    let sig = key.0.sign(&[ARTIFACT_CONTEXT, &digest].concat());
    Ok((size, hex(&digest), B64.encode(sig.to_bytes())))
}

/// Checks a downloaded file against its signed artifact entry.
pub fn verify_file(path: &Path, artifact: &Artifact, keys: &[PublicKey]) -> Result<()> {
    let (size, digest) = hash_file(path)?;
    check_digest(size, &digest, artifact, keys)
}

fn check_digest(size: u64, digest: &[u8; 32], artifact: &Artifact, keys: &[PublicKey]) -> Result<()> {
    if size != artifact.size {
        return Err(Error::Mismatch(format!("{size} bytes, expected {}", artifact.size)));
    }
    if !hex(digest).eq_ignore_ascii_case(&artifact.sha256) {
        return Err(Error::Mismatch("SHA-256 differs".into()));
    }
    verify_any(keys, &[ARTIFACT_CONTEXT, digest].concat(), &artifact.signature)
}

fn hash_file(path: &Path) -> Result<(u64, [u8; 32])> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((size, hasher.finalize().into()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// MARK: Versions and targets

pub fn parse_version(v: &str) -> Result<semver::Version> {
    semver::Version::parse(v.trim().trim_start_matches('v')).map_err(|e| Error::BadManifest(format!("version “{v}”: {e}")))
}

/// Whether `candidate` is a later version than `current` (SemVer: 1.0.0-beta < 1.0.0).
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Ok(a), Ok(b)) => a > b,
        _ => false,
    }
}

/// This build's target as manifests name it (`windows-x86_64`, `linux-aarch64`, `macos-aarch64`…).
pub fn current_target() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

// MARK: Network

/// A release newer than the running one, for this target.
#[derive(Debug, Clone)]
pub struct Update {
    pub manifest: Manifest,
    pub artifact: Artifact,
}

impl Update {
    pub fn version(&self) -> &str {
        &self.manifest.version
    }
}

/// Fetches and verifies the manifest at `feed_url`; `Some` when it offers a newer version than
/// `current_version` with an artifact of `kind` for `target`.
pub fn check(feed_url: &str, keys: &[PublicKey], current_version: &str, target: &str, kind: &str) -> Result<Option<Update>> {
    let manifest = fetch_manifest(feed_url, keys)?;
    if !is_newer(&manifest.version, current_version) {
        return Ok(None);
    }
    Ok(manifest.artifact(target, kind).cloned().map(|artifact| Update { manifest, artifact }))
}

/// Fetches and verifies the manifest at `feed_url` (http(s):// or file://).
pub fn fetch_manifest(feed_url: &str, keys: &[PublicKey]) -> Result<Manifest> {
    let bytes = if let Some(path) = feed_url.strip_prefix("file://") {
        std::fs::read(path).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { Error::NoFeed(feed_url.into()) } else { e.into() })?
    } else {
        let response = client(Duration::from_secs(30))?.get(feed_url).send().map_err(network)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NoFeed(feed_url.into()));
        }
        let response = response.error_for_status().map_err(network)?;
        // A manifest is a few KB; don't read an endless body.
        let mut bytes = Vec::new();
        response.take(1 << 20).read_to_end(&mut bytes)?;
        bytes
    };
    verify_manifest(&bytes, keys)
}

/// Downloads `update`'s artifact into `dir` and verifies it. `progress(done, total)` is called as
/// it arrives; setting `cancel` stops it. The file is only renamed into place once it's verified,
/// so a file with the final name in `dir` is always a verified one.
pub fn download(
    update: &Update,
    dir: &Path,
    keys: &[PublicKey],
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    let artifact = &update.artifact;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(file_name(&artifact.url, &update.manifest.version));
    if path.exists() && verify_file(&path, artifact, keys).is_ok() {
        return Ok(path);
    }
    let partial = path.with_extension("partial");
    let result = (|| {
        let mut response = client(Duration::from_secs(60))?.get(&artifact.url).send().map_err(network)?.error_for_status().map_err(network)?;
        let mut file = File::create(&partial)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let n = response.read(&mut buf).map_err(|e| Error::Network(e.to_string()))?;
            if n == 0 {
                break;
            }
            done += n as u64;
            if done > artifact.size {
                return Err(Error::Mismatch(format!("more than {} bytes", artifact.size)));
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
            progress(done, artifact.size);
        }
        file.sync_all()?;
        drop(file);
        check_digest(done, &hasher.finalize().into(), artifact, keys)
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&partial, &path)?;
            Ok(path)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// The last path segment of `url`, reduced to safe characters (the URL comes from a signed
/// manifest, but the name still shouldn't be able to point anywhere else).
fn file_name(url: &str, version: &str) -> String {
    let last = url.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("");
    let safe: String = last.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
    let safe = safe.trim_start_matches('.').to_string();
    if safe.is_empty() { format!("dbear-{version}-update") } else { safe }
}

fn network(e: reqwest::Error) -> Error {
    let mut text = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    Error::Network(text)
}

fn client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Network(format!("TLS setup: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let mut builder = reqwest::blocking::Client::builder()
        .use_preconfigured_tls(tls)
        .user_agent(concat!("dbear-updater/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15));
    // The blocking client applies this to sending the request and to each read of the body, so
    // a long download is fine while a stalled one fails.
    builder = builder.timeout(timeout);
    builder.build().map_err(network)
}

#[cfg(test)]
mod tests;
