//! Per-user folders, resolved the same way on macOS, Linux and Windows.

use std::path::PathBuf;

fn var(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The user's home folder: `%USERPROFILE%` on Windows (Git Bash's `HOME` is a fallback), `$HOME` elsewhere.
pub fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) { var("USERPROFILE").or_else(|| var("HOME")) } else { var("HOME") }
}

/// Where per-user app data lives:
/// - macOS: `~/Library/Application Support`
/// - Windows: `%APPDATA%` (roaming), else `~\AppData\Roaming`
/// - elsewhere: `$XDG_CONFIG_HOME` when absolute, else `~/.config`
pub fn app_data_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(home_dir()?.join("Library").join("Application Support"))
    } else if cfg!(windows) {
        var("APPDATA").or_else(|| Some(home_dir()?.join("AppData").join("Roaming")))
    } else {
        xdg("XDG_CONFIG_HOME").or_else(|| Some(home_dir()?.join(".config")))
    }
}

/// `$XDG_DATA_HOME` when absolute, else `~/.local/share` (Linux and other Unixes).
pub fn xdg_data_dir() -> Option<PathBuf> {
    xdg("XDG_DATA_HOME").or_else(|| Some(home_dir()?.join(".local").join("share")))
}

/// XDG says relative values must be ignored.
fn xdg(key: &str) -> Option<PathBuf> {
    var(key).filter(|p| p.is_absolute())
}

/// Expands a leading `~/` (and `~\` on Windows) to the home folder.
pub fn expand_home(path: &str) -> PathBuf {
    let rest = path.strip_prefix("~/").or_else(|| if cfg!(windows) { path.strip_prefix("~\\") } else { None });
    match (rest, home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// The last component of a path written with either separator (`C:\db\app.db` → `app.db`).
pub fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_accepts_both_separators() {
        assert_eq!(file_name("/Users/me/app.db"), "app.db");
        assert_eq!(file_name(r"C:\Users\me\app.db"), "app.db");
        assert_eq!(file_name("app.db"), "app.db");
        assert_eq!(file_name(""), "");
    }

    #[test]
    fn app_data_dir_is_platform_specific() {
        let Some(dir) = app_data_dir() else { return };
        if cfg!(target_os = "macos") {
            assert!(dir.ends_with("Library/Application Support"));
        } else if cfg!(windows) {
            assert!(dir.to_string_lossy().contains("AppData") || var("APPDATA").is_some());
        } else {
            assert!(dir.is_absolute());
        }
    }

    #[test]
    fn expands_only_a_leading_tilde() {
        if let Some(home) = home_dir() {
            assert_eq!(expand_home("~/a.db"), home.join("a.db"));
        }
        assert_eq!(expand_home("/tmp/~/a.db"), PathBuf::from("/tmp/~/a.db"));
        assert_eq!(expand_home("a.db"), PathBuf::from("a.db"));
    }
}
