//! Shortcut labels for the UI, written the way each platform does (⌘T on macOS, Ctrl+T elsewhere).

use gpui_kit::Keystroke;
use gpui_kit::component::kbd::Kbd;

/// `binding` uses the same syntax as the key bindings, e.g. `"secondary-enter"`.
pub fn shortcut(binding: &str) -> String {
    Keystroke::parse(binding).map(|key| Kbd::format(&key)).unwrap_or_else(|_| binding.to_string())
}

#[cfg(test)]
mod tests {
    use super::shortcut;

    #[test]
    fn uses_the_platform_modifier() {
        if cfg!(target_os = "macos") {
            assert_eq!(shortcut("secondary-t"), "⌘T");
        } else {
            assert_eq!(shortcut("secondary-t"), "Ctrl+T");
            assert_eq!(shortcut("alt-secondary-i"), "Ctrl+Alt+I");
        }
    }
}
