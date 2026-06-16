//! Read the configured push-to-talk chord from the app's own settings.
//!
//! The portal backend ([`super::portal`]) needs to know which chord to bind, but
//! nothing in the helper IPC contract carries it — the macOS/Windows helpers
//! never needed it (they hook the OS globally). So the helper reads it straight
//! out of the Electron app's `electron-store` config:
//!
//! ```text
//! ~/.config/Wispr Flow/config.json
//!   -> prefs.user.shortcuts : { "<vk+vk+...>": "<action>", ... }
//! ```
//!
//! Keys are `+`-joined Windows VK codes (left/right specific, the same codes the
//! capture path emits); values are action names. We want the entry whose action
//! is push-to-talk (`"ptt"`). Layered entries carry a leading `-1` sentinel
//! ("while PTT held"); we drop it and any other non-positive token so a layered
//! chord still yields its real keys.

use std::path::PathBuf;
use std::time::SystemTime;

/// The push-to-talk action name as stored in `prefs.user.shortcuts` values.
const PTT_ACTION: &str = "ptt";

/// Resolve `~/.config/Wispr Flow/config.json`, honoring `XDG_CONFIG_HOME`.
/// Returns None if neither `XDG_CONFIG_HOME` nor `HOME` is set (no session).
pub fn config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("Wispr Flow").join("config.json"))
}

/// Last-modified time of the config file, for cheap change polling (the portal
/// backend re-binds when this advances). None if the file is missing/unstatable.
pub fn config_mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Parse a `+`-joined VK-code shortcut key like `"162+91"` or `"-1+162+86"` into
/// its positive VK codes (the `-1` "while-PTT-held" sentinel and any other
/// non-positive token are dropped). Returns None if no positive code remains.
fn parse_chord_key(key: &str) -> Option<Vec<u32>> {
    let vks: Vec<u32> = key
        .split('+')
        .filter_map(|tok| tok.trim().parse::<i64>().ok())
        .filter(|&n| n > 0)
        .map(|n| n as u32)
        .collect();
    (!vks.is_empty()).then_some(vks)
}

/// Find the push-to-talk chord (VK codes) in a parsed config value. Scans
/// `prefs.user.shortcuts` for the entry whose action is [`PTT_ACTION`].
fn ptt_chord_from_json(root: &serde_json::Value) -> Option<Vec<u32>> {
    let shortcuts = root
        .get("prefs")?
        .get("user")?
        .get("shortcuts")?
        .as_object()?;
    for (chord, action) in shortcuts {
        if action.as_str() == Some(PTT_ACTION) {
            if let Some(vks) = parse_chord_key(chord) {
                return Some(vks);
            }
        }
    }
    None
}

/// Read and parse the push-to-talk chord from the app config at `path`.
/// None when the file is absent/unreadable/unparseable or has no PTT entry.
pub fn read_ptt_chord(path: &std::path::Path) -> Option<Vec<u32>> {
    let text = std::fs::read_to_string(path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&text).ok()?;
    ptt_chord_from_json(&root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_chord_key_handles_plain_and_layered() {
        assert_eq!(parse_chord_key("162+91"), Some(vec![162, 91]));
        // Layered "-1+..." drops the sentinel, keeps the real keys.
        assert_eq!(parse_chord_key("-1+162+86"), Some(vec![162, 86]));
        // Single key.
        assert_eq!(parse_chord_key("27"), Some(vec![27]));
        // Nothing positive -> None.
        assert_eq!(parse_chord_key("-1"), None);
        assert_eq!(parse_chord_key(""), None);
    }

    #[test]
    fn finds_ptt_entry_among_many() {
        // Mirrors the real config shape: many actions, ptt is modifier-only here.
        let cfg = json!({
            "prefs": { "user": { "shortcuts": {
                "27": "dismiss",
                "-1+162+86": "paste_last_text",
                "162+91": "ptt",
                "-1+49": "polish"
            } } }
        });
        assert_eq!(ptt_chord_from_json(&cfg), Some(vec![162, 91]));
    }

    #[test]
    fn missing_pieces_yield_none() {
        assert_eq!(ptt_chord_from_json(&json!({})), None);
        assert_eq!(
            ptt_chord_from_json(&json!({"prefs": {"user": {"shortcuts": {}}}})),
            None
        );
        // No ptt action present.
        assert_eq!(
            ptt_chord_from_json(&json!({"prefs": {"user": {"shortcuts": {"27": "dismiss"}}}})),
            None
        );
    }
}
