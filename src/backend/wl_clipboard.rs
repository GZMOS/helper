//! In-process Wayland clipboard owner (ext-data-control).
//!
//! `wl-copy` can only serve a single payload, so it can't offer a plain-text and
//! an HTML representation of the same paste simultaneously. The Windows helper
//! sets two formats at once (CF_UNICODETEXT + CF_TEXT); to match that — and to
//! offer `text/html` for rich targets — we own the selection ourselves.
//!
//! We use `ext_data_control_manager_v1` (the standardized successor to wlroots'
//! `zwlr_data_control`), which lets a non-focused client set/own the clipboard
//! without an input-event serial. KWin/wlroots/etc. advertise it. On a session
//! that lacks it, [`set_clipboard`] errors and the caller falls back to wl-copy.
//!
//! Each call spawns a detached thread that owns a Wayland connection, advertises
//! the offered MIME types, sets the selection, and serves paste requests until
//! the compositor cancels the source (i.e. something else takes the clipboard —
//! including our own next call). The call returns once the selection is
//! confirmed set (after a roundtrip), so the caller may inject Ctrl+V right away.

use std::fs::File;
use std::io::Write;
use std::sync::mpsc;
use std::time::Duration;

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{event_created_child, Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::ExtDataControlDeviceV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::ExtDataControlOfferV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::{
    Event as SourceEvent, ExtDataControlSourceV1,
};

use super::Result;

/// A MIME type plus the bytes to serve for it.
pub type Offer = (String, Vec<u8>);

struct State {
    offers: Vec<Offer>,
    cancelled: bool,
}

/// Set the clipboard to `offers` (each a `(mime, bytes)`), owning the selection
/// in-process. Blocks only until the selection is registered with the
/// compositor; serving continues on a background thread.
pub fn set_clipboard(offers: Vec<Offer>) -> Result<()> {
    if offers.is_empty() {
        return Err("no clipboard offers".into());
    }
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    std::thread::spawn(move || {
        if let Err(e) = setup_and_serve(offers, &ready_tx) {
            // If we failed before signalling ready, report it; if after, log it.
            let _ = ready_tx.send(Err(e));
        }
    });
    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(r) => r,
        Err(_) => Err("clipboard set timed out".into()),
    }
}

fn setup_and_serve(offers: Vec<Offer>, ready_tx: &mpsc::Sender<Result<()>>) -> Result<()> {
    let conn = Connection::connect_to_env().map_err(|e| format!("wl connect: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("registry init: {e}"))?;
    let qh = queue.handle();

    let manager: ExtDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("ext_data_control_manager not available: {e}"))?;
    let seat: WlSeat = globals
        .bind(&qh, 1..=9, ())
        .map_err(|e| format!("wl_seat bind: {e}"))?;

    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    for (mime, _) in &offers {
        source.offer(mime.clone());
    }
    device.set_selection(Some(&source));

    let mut state = State {
        offers,
        cancelled: false,
    };
    // Ensure the compositor has processed set_selection before we report ready
    // (so a Ctrl+V immediately after returns the new content).
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("roundtrip: {e}"))?;
    let _ = ready_tx.send(Ok(()));

    // Serve paste (`Send`) requests until the selection is taken from us.
    while !state.cancelled {
        if let Err(e) = queue.blocking_dispatch(&mut state) {
            log::debug!("clipboard dispatch ended: {e}");
            break;
        }
    }
    log::debug!("clipboard source released");
    Ok(())
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        _src: &ExtDataControlSourceV1,
        event: SourceEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            SourceEvent::Send { mime_type, fd } => {
                // A target is pasting: write the bytes for the requested type.
                let payload = state
                    .offers
                    .iter()
                    .find(|(m, _)| *m == mime_type)
                    .map(|(_, d)| d.clone());
                let mut file = File::from(fd); // OwnedFd -> File (closed on drop)
                if let Some(data) = payload {
                    if let Err(e) = file.write_all(&data) {
                        log::debug!("clipboard send write failed: {e}");
                    }
                }
            }
            SourceEvent::Cancelled => {
                // Selection replaced (by another app or our next set) -> stop.
                state.cancelled = true;
            }
            _ => {}
        }
    }
}

// The remaining objects have no events we act on, but every proxy needs a
// Dispatch impl for the queue's state type.
impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlDeviceV1,
        _: wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // We only set the selection; incoming-selection events are ignored.
    }
    // The device's `data_offer` event (opcode 0) creates a child offer object
    // describing some *other* selection; declare its user-data so wayland-client
    // can construct it (otherwise it panics), then ignore it below.
    event_created_child!(State, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        _: wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
