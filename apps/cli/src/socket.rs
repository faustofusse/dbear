//! Where `serve --socket` listens, and listening there (Unix domain sockets; Unix only for now).

use std::path::{Path, PathBuf};

/// Overrides the socket path, for the server and for the one-shot commands looking for it.
pub const SOCKET_ENV: &str = "DBEAR_SOCKET";

/// The connection store this process uses (`DBEAR_CONNECTIONS_FILE`, else the default).
pub fn store_path() -> Option<PathBuf> {
    match std::env::var_os(dbservice::CONNECTIONS_FILE_ENV).filter(|v| !v.is_empty()) {
        Some(path) => Some(PathBuf::from(path)),
        None => dbcore::store::default_path(),
    }
}

/// `$DBEAR_SOCKET`, else `serve.sock` beside the connection store (`<app data>/dbear/serve.sock`):
/// a server and a client of the same store find each other, and another store's server is never used.
pub fn default_path() -> Option<PathBuf> {
    match std::env::var_os(SOCKET_ENV).filter(|v| !v.is_empty()) {
        Some(path) => Some(PathBuf::from(path)),
        None => Some(store_path()?.with_file_name("serve.sock")),
    }
}

/// Whether two paths name the same file (they may not exist yet).
pub fn same_file(a: &Path, b: &Path) -> bool {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canonical(a) == canonical(b)
}

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use tokio::net::UnixListener;

    /// Removes the socket file when the server ends.
    pub struct Bound {
        pub listener: UnixListener,
        pub path: PathBuf,
    }

    impl Drop for Bound {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Listens on `path` (mode 0600). A socket file nobody answers on is stale (a server that
    /// crashed) and is replaced; one that answers means a server is already running.
    pub async fn bind(path: &Path) -> io::Result<Bound> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        if std::fs::symlink_metadata(path).is_ok() {
            if tokio::net::UnixStream::connect(path).await.is_ok() {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("a server is already listening on {}", path.display())));
            }
            std::fs::remove_file(path)?;
        }
        // Created without group/other access from the start (umask), then made exactly 0600.
        // SAFETY: umask only swaps the process's file mode mask.
        let old = unsafe { libc::umask(0o177) };
        let listener = UnixListener::bind(path);
        unsafe { libc::umask(old) };
        let listener = listener?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Bound { listener, path: path.to_path_buf() })
    }
}
