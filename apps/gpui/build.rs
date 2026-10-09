//! Build-time settings for the app:
//! - `DBEAR_VERSION`: the release version (CI passes the tag's), else the crate's.
//! - `DBEAR_UPDATE_PUBLIC_KEY`: the ed25519 key(s) updates must be signed with; from the variable
//!   of the same name, else `packaging/update-public-key`. Empty: the updater is off.
//! - Windows: the icon and version information of `dbear.exe`.

use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest_dir.join("../..");

    println!("cargo:rerun-if-env-changed=DBEAR_VERSION");
    let version = std::env::var("DBEAR_VERSION")
        .ok()
        .map(|v| v.trim().trim_start_matches('v').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap());
    println!("cargo:rustc-env=DBEAR_VERSION={version}");

    let key_file = root.join("packaging/update-public-key");
    println!("cargo:rerun-if-env-changed=DBEAR_UPDATE_PUBLIC_KEY");
    println!("cargo:rerun-if-changed={}", key_file.display());
    let keys = std::env::var("DBEAR_UPDATE_PUBLIC_KEY").ok().filter(|k| !k.trim().is_empty()).unwrap_or_else(|| {
        std::fs::read_to_string(&key_file)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join(",")
    });
    println!("cargo:rustc-env=DBEAR_UPDATE_PUBLIC_KEY={}", keys.trim());

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources(&root, &version);
    }
}

/// `dbear.exe`'s icon and the version Explorer shows. GPUI embeds the manifest (DPI awareness,
/// common controls) itself.
fn windows_resources(root: &Path, version: &str) {
    let icon = root.join("packaging/windows/dbear.ico");
    assert!(icon.exists(), "missing {} (scripts/make-windows-icon.sh)", icon.display());
    println!("cargo:rerun-if-changed={}", icon.display());
    // rc.exe wants backslashes (escaped in its strings); llvm-rc on macOS/Linux takes the path as is.
    let icon = icon.display().to_string();
    let icon = if cfg!(windows) { icon.replace('/', "\\").replace('\\', "\\\\") } else { icon };
    let numbers: Vec<u16> = version.split(['-', '+']).next().unwrap_or("0").split('.').map(|n| n.parse().unwrap_or(0)).collect();
    let n = |i: usize| numbers.get(i).copied().unwrap_or(0);
    let numeric = format!("{},{},{},0", n(0), n(1), n(2));
    let rc = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Fausto Fusse"
      VALUE "FileDescription", "dbear"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "dbear"
      VALUE "LegalCopyright", "Copyright (c) 2026 Fausto Fusse. MIT License."
      VALUE "OriginalFilename", "dbear.exe"
      VALUE "ProductName", "dbear"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
    );
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("dbear.rc");
    std::fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_optional().unwrap();
}
