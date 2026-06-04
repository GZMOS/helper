//! XInput2 key capture — global raw key events on a true X11 session.
//!
//! Selects `XI_RawKeyPress`/`XI_RawKeyRelease` on the root window: events arrive
//! regardless of focus, with no grab and **no device access** — just an X
//! connection. The raw event `detail` is the X keycode, which on Linux X servers
//! is the evdev code + 8 (the XKB/evdev convention), so we subtract 8 and reuse
//! `keymap::evdev_to_vk` — the exact same translation the evdev path uses, so
//! both backends converge on identical VK codes.
//!
//! Not used on Wayland: under XWayland, raw events only cover XWayland surfaces,
//! not global input — the caller gates this on a true X11 session.

use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xinput::{self, ConnectionExt as _, EventMask, KeyEventFlags, XIEventMask};
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use super::{emit_keypress, HeldKeys};
use crate::backend::EventSink;
use crate::keymap;

// X11 keycodes are evdev codes + 8 on Linux (XKB/evdev convention).
const X_KEYCODE_OFFSET: u32 = 8;
// XIAllMasterDevices — raw events are delivered via the master devices.
const XI_ALL_MASTER_DEVICES: xinput::DeviceId = 1;

/// Connect, negotiate XI2, select raw key events, and spawn the reader. Returns
/// a [`HeldKeys`] handle (backed by a second connection) or an error so the
/// caller can fall back to evdev.
pub fn start(events: EventSink) -> Result<Box<dyn HeldKeys>, String> {
    // Reader connection: owns the blocking event loop. A second connection backs
    // held-key queries, since the reader is parked in `wait_for_event`.
    let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("x11 connect: {e}"))?;
    let root = conn.setup().roots[screen_num].root;

    conn.extension_information(xinput::X11_EXTENSION_NAME)
        .map_err(|e| format!("xinput query: {e}"))?
        .ok_or("XInput extension not available on this X server")?;
    // XI2 must be negotiated before raw events are delivered.
    conn.xinput_xi_query_version(2, 0)
        .map_err(|e| format!("xi_query_version: {e}"))?
        .reply()
        .map_err(|e| format!("xi_query_version reply: {e}"))?;

    let mask = EventMask {
        deviceid: XI_ALL_MASTER_DEVICES,
        mask: vec![XIEventMask::RAW_KEY_PRESS | XIEventMask::RAW_KEY_RELEASE],
    };
    conn.xinput_xi_select_events(root, &[mask])
        .map_err(|e| format!("xi_select_events: {e}"))?;
    conn.flush().map_err(|e| format!("flush: {e}"))?;

    let (qconn, _) = x11rb::connect(None).map_err(|e| format!("x11 query connect: {e}"))?;

    let index = Arc::new(AtomicU64::new(0));
    let pid = std::process::id();
    std::thread::Builder::new()
        .name("key-capture-xinput".to_string())
        .spawn(move || read_loop(conn, &events, &index, pid))
        .map_err(|e| format!("spawn xinput reader: {e}"))?;

    Ok(Box::new(XinputHeld { conn: qconn }))
}

fn read_loop(conn: RustConnection, events: &EventSink, index: &AtomicU64, pid: u32) {
    let repeat = u32::from(KeyEventFlags::KEY_REPEAT);
    loop {
        let event = match conn.wait_for_event() {
            Ok(e) => e,
            Err(e) => {
                log::info!("xinput capture: event stream ended: {e}");
                return;
            }
        };
        let (detail, flags, press) = match event {
            Event::XinputRawKeyPress(e) => (e.detail, e.flags, true),
            Event::XinputRawKeyRelease(e) => (e.detail, e.flags, false),
            _ => continue,
        };
        if press && u32::from(flags) & repeat != 0 {
            continue; // auto-repeat
        }
        if let Some(vk) = xkeycode_to_vk(detail) {
            emit_keypress(events, index, pid, vk, press);
        }
    }
}

/// X keycode -> Windows VK, via evdev (keycode - 8). None if out of range or
/// unmapped.
fn xkeycode_to_vk(detail: u32) -> Option<u32> {
    let evdev = u16::try_from(detail.checked_sub(X_KEYCODE_OFFSET)?).ok()?;
    keymap::evdev_to_vk(evdev)
}

/// Stale-key querier backed by the core-X `QueryKeymap` (a 256-bit keycode
/// bitmap) — no device access, mirrors the evdev `EVIOCGKEY` path.
struct XinputHeld {
    conn: RustConnection,
}

impl HeldKeys for XinputHeld {
    fn held_vks(&self) -> HashSet<u32> {
        let mut held = HashSet::new();
        let keys = match self.conn.query_keymap() {
            Ok(cookie) => match cookie.reply() {
                Ok(r) => r.keys,
                Err(e) => {
                    log::debug!("query_keymap reply: {e}");
                    return held;
                }
            },
            Err(e) => {
                log::debug!("query_keymap: {e}");
                return held;
            }
        };
        // keys[b] bit i set => X keycode (b*8 + i) is pressed.
        for (byte_idx, &byte) in keys.iter().enumerate() {
            if byte == 0 {
                continue;
            }
            for bit in 0..8u32 {
                if (byte >> bit) & 1 == 1 {
                    let keycode = (byte_idx as u32) * 8 + bit;
                    if let Some(vk) = xkeycode_to_vk(keycode) {
                        held.insert(vk);
                    }
                }
            }
        }
        held
    }
}
