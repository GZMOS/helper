// Wispr Flow Window Bridge — GNOME Shell extension (GNOME 45+, ESM modules).
//
// Runs inside the privileged gnome-shell process and exports a small D-Bus
// interface giving the focused window's identity plus a focus-change signal.
// This is the "Window Calls" / "Focused Window D-Bus" pattern: the built-in
// org.gnome.Shell.Introspect API is gated behind unsafe_mode on modern GNOME
// (AccessDenied to normal callers), so the Wispr Flow Linux helper bundles and
// enables this extension instead and talks to it over the session bus.
//
// Interface:
//   name      : org.wispr.flow.WindowBridge   (well-known bus name we own)
//   object    : /org/wispr/flow/WindowBridge
//   interface : org.wispr.flow.WindowBridge
//     GetFocusedWindow() -> (s)   JSON {appId,title,pid,wmClass} ("" if none)
//     GetWindowList()    -> (s)   JSON array of the same shape (normal windows)
//     FocusChanged(s)             signal carrying the focused-window JSON

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'org.wispr.flow.WindowBridge';
const OBJECT_PATH = '/org/wispr/flow/WindowBridge';

const IFACE_XML = `
<node>
  <interface name="org.wispr.flow.WindowBridge">
    <method name="GetFocusedWindow">
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="GetWindowList">
      <arg type="s" direction="out" name="json"/>
    </method>
    <signal name="FocusChanged">
      <arg type="s" name="json"/>
    </signal>
  </interface>
</node>`;

// The focused window, or — when mutter reports none (focus on the shell /
// overview, or a session that hasn't received real input yet) — the
// most-recently-used normal, taskbar-visible window. For a dictation helper the
// MRU window is the right "where would my text go" answer when nothing holds
// keyboard focus; mirrors how the macOS/Windows helpers keep a last-active app.
function focusedOrMru() {
    const win = global.display.get_focus_window();
    if (win)
        return win;
    try {
        const ws = global.workspace_manager.get_active_workspace();
        const list = global.display.get_tab_list(Meta.TabList.NORMAL, ws);
        for (const w of list) {
            if (w && w.window_type === Meta.WindowType.NORMAL && !w.is_skip_taskbar())
                return w;
        }
    } catch (e) {}
    return null;
}

// Build the {appId,title,pid,wmClass} object for a Meta.Window, resolving the
// real .desktop app id via Shell.WindowTracker.
function windowInfo(win) {
    if (!win)
        return {appId: '', title: '', pid: 0, wmClass: ''};

    let appId = '';
    try {
        const tracker = Shell.WindowTracker.get_default();
        const app = tracker.get_window_app(win);
        if (app)
            appId = app.get_id() ?? '';
    } catch (e) {
        // get_window_app can fail transiently while a window is mapping.
    }

    let title = '';
    try {
        title = win.get_title() ?? '';
    } catch (e) {}

    let pid = 0;
    try {
        pid = win.get_pid() ?? 0;
        if (pid < 0)
            pid = 0;
    } catch (e) {}

    let wmClass = '';
    try {
        wmClass = win.get_wm_class() ?? '';
    } catch (e) {}

    return {appId, title, pid, wmClass};
}

export default class WindowBridgeExtension extends Extension {
    enable() {
        this._dbusImpl = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        this._dbusImpl.export(Gio.DBus.session, OBJECT_PATH);
        this._ownerId = Gio.bus_own_name_on_connection(
            Gio.DBus.session,
            BUS_NAME,
            Gio.BusNameOwnerFlags.REPLACE,
            null,
            null
        );

        // Track the focused window so we can also fire FocusChanged on title
        // changes of the currently-focused window (mirrors macOS/Windows which
        // surface title updates as focus updates).
        this._focusWindow = null;
        this._titleId = 0;

        this._focusChangedId = global.display.connect(
            'notify::focus-window',
            () => this._onFocusChanged()
        );

        // Seed the title-watch on whatever already has focus.
        this._onFocusChanged();
    }

    disable() {
        if (this._focusChangedId) {
            global.display.disconnect(this._focusChangedId);
            this._focusChangedId = 0;
        }
        this._disconnectTitle();
        this._focusWindow = null;

        if (this._ownerId) {
            Gio.bus_unown_name(this._ownerId);
            this._ownerId = 0;
        }
        if (this._dbusImpl) {
            this._dbusImpl.unexport();
            this._dbusImpl = null;
        }
    }

    _disconnectTitle() {
        if (this._titleId && this._focusWindow) {
            try {
                this._focusWindow.disconnect(this._titleId);
            } catch (e) {}
        }
        this._titleId = 0;
    }

    _onFocusChanged() {
        const win = global.display.get_focus_window();

        // Re-arm the title watcher on the newly focused window.
        if (win !== this._focusWindow) {
            this._disconnectTitle();
            this._focusWindow = win;
            if (win) {
                this._titleId = win.connect('notify::title', () =>
                    this._emitFocus()
                );
            }
        }

        this._emitFocus();
    }

    _emitFocus() {
        if (!this._dbusImpl)
            return;
        const json = JSON.stringify(windowInfo(focusedOrMru()));
        this._dbusImpl.emit_signal(
            'FocusChanged',
            new GLib.Variant('(s)', [json])
        );
    }

    // D-Bus method: focused window identity as JSON (MRU fallback when none).
    GetFocusedWindow() {
        return JSON.stringify(windowInfo(focusedOrMru()));
    }

    // D-Bus method: all normal, taskbar-visible windows as a JSON array. The
    // skip_taskbar filter drops docks/OSDs and — importantly — the helper's own
    // wl-copy clipboard surfaces (which map as titled "wl-clipboard" windows when
    // the in-process ext-data-control path is unavailable, e.g. on mutter), so
    // they don't pollute GetRunningApps. Matches the KWin script's filter.
    GetWindowList() {
        const out = [];
        for (const actor of global.get_window_actors()) {
            const win = actor.meta_window;
            if (!win)
                continue;
            if (win.window_type !== Meta.WindowType.NORMAL)
                continue;
            if (win.is_skip_taskbar())
                continue;
            const info = windowInfo(win);
            // Drop windows with no real application identity: the Shell assigns a
            // synthetic "window:N" app id and an empty wmClass to surfaces with no
            // .desktop file — e.g. the helper's own wl-copy clipboard surfaces on
            // mutter (which has no ext-data-control). Real apps set a wmClass.
            if (info.wmClass === '' && info.appId.startsWith('window:'))
                continue;
            out.push(info);
        }
        return JSON.stringify(out);
    }
}
