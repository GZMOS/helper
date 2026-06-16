//! GlobalShortcuts portal capture — the compositor-mediated, device-free path.
//!
//! Instead of reading `/dev/input` (a keylogging surface — see [`super::evdev`]),
//! register the push-to-talk chord with `org.freedesktop.portal.GlobalShortcuts`
//! and let the compositor own the grab. We never see other keystrokes; we only
//! get `Activated` / `Deactivated` signals for *our* shortcut. That's the whole
//! security win, and it's why this is the default on Wayland.
//!
//! ## How it maps onto the app's model
//!
//! The app has no hotkey detection of its own — it matches the
//! [`KeypressEvent`](super) stream against `prefs.user.shortcuts`. So when our
//! shortcut fires we **synthesize that stream**: on `Activated` emit a press for
//! every VK in the chord, on `Deactivated` emit the releases. We bind the
//! trigger derived from the *same* chord we synthesize, so what the compositor
//! grabs and what the app matches stay identical.
//!
//! ## Compositor coverage
//!
//! The interface is portable (KDE/KWin, GNOME/Mutter, Hyprland). wlroots
//! (`xdg-desktop-portal-wlr`, e.g. sway) does **not** implement it — there
//! `start` returns an error and the caller surfaces it (no silent fallback;
//! `WISPR_CAPTURE=evdev` is the opt-in). KDE additionally requires a real app
//! identity (systemd app scope) and rejects modifier-only triggers; both are
//! handled here (the latter as a clear error) and documented in `--doctor`.
//!
//! ## Threading / runtime
//!
//! Like [`crate::backend::kwin`], zbus's `tokio` feature is enabled tree-wide, so
//! signals are only dispatched while a runtime drives the connection. We run the
//! session + signal loop on a dedicated current-thread tokio runtime that lives
//! for the process; `start` blocks on a readiness channel so its return value
//! reflects whether binding actually succeeded.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::Proxy;

use super::{config, emit_keypress, HeldKeys};
use crate::backend::EventSink;
use crate::keymap;

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const GS_IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";

/// Our shortcut id within the session. The compositor echoes it back in
/// `Activated` / `Deactivated`, so we match on it.
const SHORTCUT_ID: &str = "ptt";

/// How often to re-stat the app config so a chord change (e.g. via the in-app
/// recorder) triggers a re-bind without restarting the helper.
const CONFIG_POLL: Duration = Duration::from_secs(3);

/// Start GlobalShortcuts capture. Returns a [`HeldKeys`] handle immediately and
/// does the connect/bind on the runtime thread, logging the outcome.
///
/// Returns `Err` only for the few failures knowable synchronously (thread spawn,
/// no config dir, an unbindable chord) so the caller can word its "capture is
/// off" message precisely. Bind failures that depend on the compositor (portal
/// absent, denied, slow approval) are logged from the thread — we don't block
/// startup on a `BindShortcuts` dialog that GNOME always shows and the user may
/// take a while to approve. There is no fallback to evdev either way.
pub fn start(events: EventSink) -> Result<Box<dyn HeldKeys>, String> {
    let cfg_path = config::config_path()
        .ok_or("no config dir (HOME/XDG_CONFIG_HOME unset)")?;
    // Fail fast on an unbindable chord (modifier-only): there's nothing the
    // runtime can do about it and the caller's message should say so.
    let chord = resolve_chord(&cfg_path)?;

    let held: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
    let index = Arc::new(AtomicU64::new(0));
    let pid = std::process::id();

    let thread_held = held.clone();
    std::thread::Builder::new()
        .name("key-capture-portal".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("key capture: portal tokio runtime failed: {e}");
                    return;
                }
            };
            rt.block_on(run_portal(events, thread_held, index, pid, cfg_path, chord));
        })
        .map_err(|e| format!("spawn portal thread: {e}"))?;

    Ok(Box::new(PortalHeld { held }))
}

/// Runtime-thread body: connect, bind the chord, then loop on the signal streams
/// (and a config-mtime poll for re-binds) for the process lifetime.
async fn run_portal(
    events: EventSink,
    held: Arc<Mutex<HashSet<u32>>>,
    index: Arc<AtomicU64>,
    pid: u32,
    cfg_path: std::path::PathBuf,
    mut chord: Vec<u32>,
) {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            log::error!("key capture: portal session bus failed: {e}");
            return;
        }
    };
    let unique = match conn.unique_name() {
        Some(n) => n.as_str().to_string(),
        None => {
            log::error!("key capture: portal session bus has no unique name");
            return;
        }
    };
    let gs = match Proxy::new(&conn, PORTAL_DEST, PORTAL_PATH, GS_IFACE).await {
        Ok(p) => p,
        Err(e) => {
            log::error!("key capture: GlobalShortcuts proxy failed: {e}");
            return;
        }
    };
    let mut cfg_mtime = config::config_mtime(&cfg_path);

    // Subscribe to the per-shortcut signals before binding.
    let mut activated = match gs.receive_signal("Activated").await {
        Ok(s) => s,
        Err(e) => {
            log::error!("key capture: subscribe Activated failed: {e}");
            return;
        }
    };
    let mut deactivated = match gs.receive_signal("Deactivated").await {
        Ok(s) => s,
        Err(e) => {
            log::error!("key capture: subscribe Deactivated failed: {e}");
            return;
        }
    };

    // Create the session and bind. Failure here is the portal-absent case
    // (wlroots) or a denied chord; capture stays off until WISPR_CAPTURE=evdev.
    let mut session = match create_and_bind(&conn, &gs, &unique, &chord).await {
        Ok(s) => s,
        Err(e) => {
            log::error!(
                "key capture: GlobalShortcuts bind failed ({e}). Push-to-talk and \
                 the shortcut recorder are OFF. On a compositor without portal \
                 support (sway/wlroots), set WISPR_CAPTURE=evdev."
            );
            return;
        }
    };
    log::info!("key capture: GlobalShortcuts portal active (chord {chord:?} bound)");

    let mut poll = tokio::time::interval(CONFIG_POLL);
    loop {
        tokio::select! {
            Some(msg) = activated.next() => {
                log::debug!("portal: Activated signal received");
                if signal_matches(&msg, &session) {
                    set_chord(&events, &index, pid, &held, &chord, true);
                }
            }
            Some(msg) = deactivated.next() => {
                log::debug!("portal: Deactivated signal received");
                if signal_matches(&msg, &session) {
                    set_chord(&events, &index, pid, &held, &chord, false);
                }
            }
            _ = poll.tick() => {
                let now = config::config_mtime(&cfg_path);
                if now != cfg_mtime {
                    cfg_mtime = now;
                    if let Some(new) = rebind_on_change(&conn, &gs, &unique, &cfg_path, &chord, &session).await {
                        chord = new.0;
                        session = new.1;
                    }
                }
            }
        }
    }
}

/// Read the PTT chord from config and confirm it's bindable as a portal trigger.
fn resolve_chord(cfg_path: &std::path::Path) -> Result<Vec<u32>, String> {
    let chord = config::read_ptt_chord(cfg_path).ok_or_else(|| {
        "no push-to-talk shortcut found in the app config; set one in Wispr Flow first".to_string()
    })?;
    if keymap::chord_to_xdg_trigger(&chord).is_none() {
        return Err(format!(
            "push-to-talk chord {chord:?} can't be a portal trigger (modifier-only \
             chords are rejected by KDE). Set a chord that includes a regular key, \
             or use WISPR_CAPTURE=evdev."
        ));
    }
    Ok(chord)
}

/// CreateSession + BindShortcuts. Returns the bound session's object path.
async fn create_and_bind(
    conn: &zbus::Connection,
    gs: &Proxy<'_>,
    unique: &str,
    chord: &[u32],
) -> Result<OwnedObjectPath, String> {
    let trigger = keymap::chord_to_xdg_trigger(chord)
        .ok_or("internal: chord not bindable (should have been caught earlier)")?;

    // --- CreateSession ---
    let create_token = "wf_create";
    let session_token = "wf_session";
    let mut create_opts: HashMap<&str, Value> = HashMap::new();
    create_opts.insert("handle_token", Value::from(create_token));
    create_opts.insert("session_handle_token", Value::from(session_token));
    let results = portal_call(conn, gs, unique, create_token, "CreateSession", &(create_opts,))
        .await
        .map_err(|e| format!("CreateSession ({e}) — GlobalShortcuts portal may be unavailable"))?;
    let session_handle = results
        .get("session_handle")
        .and_then(owned_to_string)
        .ok_or("CreateSession: no session_handle in results")?;
    let session_path = ObjectPath::try_from(session_handle)
        .map_err(|e| format!("session_handle not an object path: {e}"))?;

    // --- BindShortcuts ---
    let bind_token = "wf_bind";
    let mut shortcut_meta: HashMap<&str, Value> = HashMap::new();
    shortcut_meta.insert("description", Value::from("Wispr Flow push-to-talk"));
    shortcut_meta.insert("preferred_trigger", Value::from(trigger.as_str()));
    let shortcuts: Vec<(&str, HashMap<&str, Value>)> = vec![(SHORTCUT_ID, shortcut_meta)];
    let mut bind_opts: HashMap<&str, Value> = HashMap::new();
    bind_opts.insert("handle_token", Value::from(bind_token));
    let _ = portal_call(
        conn,
        gs,
        unique,
        bind_token,
        "BindShortcuts",
        &(&session_path, shortcuts, "", bind_opts),
    )
    .await
    .map_err(|e| format!("BindShortcuts: {e}"))?;

    Ok(session_path.into())
}

/// Invoke a portal method that follows the Request/Response pattern: subscribe to
/// the request's `Response` signal (path derived from our unique name + the
/// handle token), make the call, await the response, return its results dict.
async fn portal_call<B>(
    conn: &zbus::Connection,
    gs: &Proxy<'_>,
    unique: &str,
    handle_token: &str,
    method: &str,
    body: &B,
) -> Result<HashMap<String, OwnedValue>, String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let req_path = request_path(unique, handle_token);
    let req_proxy = Proxy::new(conn, PORTAL_DEST, req_path.as_str(), REQUEST_IFACE)
        .await
        .map_err(|e| format!("request proxy: {e}"))?;
    let mut responses = req_proxy
        .receive_signal("Response")
        .await
        .map_err(|e| format!("subscribe Response: {e}"))?;

    // The method reply is just the (server-chosen) request handle; we drive off
    // the Response signal at the path we pre-computed, so ignore the value.
    gs.call::<_, _, OwnedObjectPath>(method, body)
        .await
        .map_err(|e| format!("{method} call: {e}"))?;

    let msg = responses
        .next()
        .await
        .ok_or_else(|| format!("{method}: response stream ended"))?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg
        .body()
        .deserialize()
        .map_err(|e| format!("{method} response decode: {e}"))?;
    match code {
        0 => Ok(results),
        1 => Err(format!("{method} cancelled by user")),
        other => Err(format!("{method} failed (response code {other})")),
    }
}

/// Re-read the config after an mtime change and, if the chord changed and is
/// still bindable, close the old session and bind the new chord. Returns the new
/// `(chord, session)` on success, None to keep the current binding.
async fn rebind_on_change(
    conn: &zbus::Connection,
    gs: &Proxy<'_>,
    unique: &str,
    cfg_path: &std::path::Path,
    current: &[u32],
    old_session: &OwnedObjectPath,
) -> Option<(Vec<u32>, OwnedObjectPath)> {
    let new_chord = match resolve_chord(cfg_path) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("portal re-bind skipped: {e}");
            return None;
        }
    };
    if new_chord == current {
        return None;
    }
    log::info!("portal: PTT chord changed {current:?} -> {new_chord:?}, re-binding");
    close_session(conn, old_session).await;
    match create_and_bind(conn, gs, unique, &new_chord).await {
        Ok(session) => Some((new_chord, session)),
        Err(e) => {
            log::warn!("portal re-bind failed: {e}");
            None
        }
    }
}

/// Close a portal session (best-effort; logged at debug on failure).
async fn close_session(conn: &zbus::Connection, session: &OwnedObjectPath) {
    match Proxy::new(conn, PORTAL_DEST, session, SESSION_IFACE).await {
        Ok(p) => {
            if let Err(e) = p.call::<_, _, ()>("Close", &()).await {
                log::debug!("portal session Close: {e}");
            }
        }
        Err(e) => log::debug!("portal session proxy: {e}"),
    }
}

/// True if an `Activated`/`Deactivated` signal is for our session + shortcut id.
fn signal_matches(msg: &zbus::Message, session: &OwnedObjectPath) -> bool {
    // Body: (o session_handle, s shortcut_id, t timestamp, a{sv} options).
    match msg
        .body()
        .deserialize::<(OwnedObjectPath, String, u64, HashMap<String, OwnedValue>)>()
    {
        Ok((sig_session, shortcut_id, _, _)) => {
            sig_session.as_ref() == session.as_ref() && shortcut_id == SHORTCUT_ID
        }
        Err(e) => {
            log::debug!("portal signal decode: {e}");
            false
        }
    }
}

/// Synthesize the chord as a press or release: emit a `KeypressEvent` per VK and
/// update the held-keys set (presses in order, releases in reverse).
fn set_chord(
    events: &EventSink,
    index: &AtomicU64,
    pid: u32,
    held: &Arc<Mutex<HashSet<u32>>>,
    chord: &[u32],
    press: bool,
) {
    let order: Vec<u32> = if press {
        chord.to_vec()
    } else {
        chord.iter().rev().copied().collect()
    };
    for vk in order {
        emit_keypress(events, index, pid, vk, press);
        if let Ok(mut set) = held.lock() {
            if press {
                set.insert(vk);
            } else {
                set.remove(&vk);
            }
        }
    }
}

/// The request object path the portal will use for a call with this handle
/// token: `/org/freedesktop/portal/desktop/request/<SENDER>/<token>`, where
/// SENDER is our unique name with the leading ':' stripped and '.' -> '_'.
/// Documented by the portal spec so clients can subscribe before calling.
fn request_path(unique: &str, token: &str) -> String {
    let sender = unique.trim_start_matches(':').replace('.', "_");
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

/// Best-effort extraction of a `String` from an `a{sv}` value.
fn owned_to_string(v: &OwnedValue) -> Option<String> {
    <&str>::try_from(&**v).ok().map(str::to_string)
}

/// Stale-key querier backed by the tracked activation state. Mirrors evdev's
/// `EVIOCGKEY`: a key the app thinks is held but that isn't in our set has been
/// released (the portal told us so via `Deactivated`).
struct PortalHeld {
    held: Arc<Mutex<HashSet<u32>>>,
}

impl HeldKeys for PortalHeld {
    fn held_vks(&self) -> HashSet<u32> {
        self.held.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_path_matches_portal_convention() {
        // Leading ':' stripped, '.' -> '_', token appended.
        assert_eq!(
            request_path(":1.407", "wf_create"),
            "/org/freedesktop/portal/desktop/request/1_407/wf_create"
        );
    }
}
