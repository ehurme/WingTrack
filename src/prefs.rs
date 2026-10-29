//! Per-user settings that must never travel in share links (API keys): kept in the
//! browser's localStorage on the web, and in a small file in the user's config folder on
//! desktop. Failures are silent: the app simply forgets the value.

const PREFIX: &str = "wingtrack.";

#[cfg(target_arch = "wasm32")]
pub fn load(name: &str) -> Option<String> {
    let s = web_sys::window()?.local_storage().ok()??;
    s.get_item(&format!("{PREFIX}{name}")).ok()?
}

#[cfg(target_arch = "wasm32")]
pub fn save(name: &str, value: &str) {
    if let Some(Ok(Some(s))) = web_sys::window().map(|w| w.local_storage()) {
        let key = format!("{PREFIX}{name}");
        let _ = if value.is_empty() {
            s.remove_item(&key)
        } else {
            s.set_item(&key, value)
        };
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn path(name: &str) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(std::path::PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("wingtrack").join(format!("{PREFIX}{name}")))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load(name: &str) -> Option<String> {
    let s = std::fs::read_to_string(path(name)?).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn save(name: &str, value: &str) {
    let Some(p) = path(name) else { return };
    if value.is_empty() {
        let _ = std::fs::remove_file(p);
        return;
    }
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, value);
}
