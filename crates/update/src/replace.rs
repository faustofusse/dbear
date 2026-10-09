//! Updating a copy that wasn't installed (the portable zip): the running executable is swapped
//! for the one in a verified zip, in place, without an installer or administrator rights.
//!
//! Windows can't overwrite or delete a running `.exe`, but it can rename it. So:
//! 1. the new executable is extracted beside the current one as `<name>.new<ext>`;
//! 2. the running `<name><ext>` is renamed to `<name>.old<ext>`;
//! 3. `<name>.new<ext>` is renamed to `<name><ext>` (on failure, step 2 is undone).
//!
//! The next launch removes `<name>.old<ext>` ([`remove_old`]). The same steps work on Linux and
//! macOS (where the running file could simply be replaced), so other platforms can reuse this.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Largest executable accepted from a zip: a sanity bound, the zip itself is already verified.
const MAX_EXE_SIZE: u64 = 512 << 20;

fn sibling(exe: &Path, tag: &str) -> PathBuf {
    let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("dbear");
    match exe.extension().and_then(|e| e.to_str()) {
        Some(ext) => exe.with_file_name(format!("{stem}.{tag}.{ext}")),
        None => exe.with_file_name(format!("{stem}.{tag}")),
    }
}

/// Where the previous executable is left after [`replace_from_zip`].
pub fn old_path(exe: &Path) -> PathBuf {
    sibling(exe, "old")
}

/// Whether `exe`'s folder can be written (so this copy can replace itself).
pub fn can_replace(exe: &Path) -> bool {
    let Some(dir) = exe.parent() else { return false };
    let probe = dir.join(format!(".dbear-write-test-{}", std::process::id()));
    match File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Replaces `exe` (normally the running executable) with the file named `entry` in `zip`, which
/// must already be verified. The old executable stays as [`old_path`] until [`remove_old`].
pub fn replace_from_zip(zip: &Path, entry: &str, exe: &Path) -> io::Result<()> {
    let new = sibling(exe, "new");
    let old = old_path(exe);
    extract(zip, entry, &new)?;
    // A leftover from an earlier update (its process may still run: then this fails, and so do we).
    if old.exists() {
        std::fs::remove_file(&old)?;
    }
    if let Err(e) = std::fs::rename(exe, &old) {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&new, exe) {
        // Put the old one back so the app still starts.
        let _ = std::fs::rename(&old, exe);
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    Ok(())
}

fn extract(zip: &Path, entry: &str, dest: &Path) -> io::Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(zip)?).map_err(io::Error::other)?;
    let mut file = archive.by_name(entry).map_err(|e| io::Error::other(format!("{entry} in the update: {e}")))?;
    if file.size() > MAX_EXE_SIZE || file.size() == 0 {
        return Err(io::Error::other(format!("{entry} in the update has an unexpected size ({} bytes)", file.size())));
    }
    let result = (|| {
        let mut out = File::create(dest)?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut written = 0u64;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > MAX_EXE_SIZE {
                return Err(io::Error::other("the update's executable is too large"));
            }
            out.write_all(&buf[..n])?;
        }
        out.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

/// Removes the executable a previous update left behind (and a half-extracted one). `wait`: keep
/// trying that long while it's still in use, i.e. the previous process hasn't exited yet (after a
/// restart to update). Returns whether nothing is left.
pub fn remove_old(exe: &Path, wait: Duration) -> bool {
    let _ = std::fs::remove_file(sibling(exe, "new"));
    let old = old_path(exe);
    let start = Instant::now();
    loop {
        match std::fs::remove_file(&old) {
            Ok(()) => return true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return true,
            Err(_) if start.elapsed() < wait => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_with(path: &Path, entries: &[(&str, &[u8])]) {
        let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, bytes) in entries {
            writer.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn replaces_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("dbear.exe");
        std::fs::write(&exe, b"old").unwrap();
        let zip = dir.path().join("update.zip");
        zip_with(&zip, &[("dbear.exe", b"new version")]);

        assert!(can_replace(&exe));
        replace_from_zip(&zip, "dbear.exe", &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new version");
        assert_eq!(old_path(&exe), dir.path().join("dbear.old.exe"));
        assert_eq!(std::fs::read(old_path(&exe)).unwrap(), b"old");
        assert!(!dir.path().join("dbear.new.exe").exists());

        // Again (an old one is still there from the first time).
        zip_with(&zip, &[("dbear.exe", b"newer")]);
        replace_from_zip(&zip, "dbear.exe", &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"newer");

        assert!(remove_old(&exe, Duration::ZERO));
        assert!(!old_path(&exe).exists());
        assert!(remove_old(&exe, Duration::ZERO), "nothing to remove is fine");
    }

    #[test]
    fn leaves_the_app_alone_on_errors() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("dbear.exe");
        std::fs::write(&exe, b"old").unwrap();
        let zip = dir.path().join("update.zip");

        zip_with(&zip, &[("readme.txt", b"no exe here")]);
        assert!(replace_from_zip(&zip, "dbear.exe", &exe).is_err());
        zip_with(&zip, &[("dbear.exe", b"")]);
        assert!(replace_from_zip(&zip, "dbear.exe", &exe).is_err());
        std::fs::write(&zip, b"not a zip").unwrap();
        assert!(replace_from_zip(&zip, "dbear.exe", &exe).is_err());

        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "only dbear.exe and the zip: {names:?}");
    }

    #[test]
    fn names() {
        assert_eq!(sibling(Path::new("/a/dbear.exe"), "new"), Path::new("/a/dbear.new.exe"));
        assert_eq!(sibling(Path::new("/a/dbear"), "old"), Path::new("/a/dbear.old"));
    }
}
