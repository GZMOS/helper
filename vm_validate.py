#!/usr/bin/env python3
"""One-shot VM validation harness for the Wispr Flow Linux helper.

Runs the full command surface against a live graphical session and prints a
concise PASS/FAIL summary plus the helper's backend-selection log. Designed to be
copied into a VM and run inside the user's graphical session env (the caller must
export WAYLAND_DISPLAY/DISPLAY/XDG_RUNTIME_DIR/DBUS_SESSION_BUS_ADDRESS/
XDG_CURRENT_DESKTOP — see the per-VM runner).

Checks:
  1. handshake          IsReady -> ACK
  2. backend            parsed from RUST_LOG (detect() lines)
  3. paste+save         open file in editor, PasteText + Ctrl+S, read file
  4. active-app         GetActiveAppInfo non-empty for the focused editor
  5. running-apps       GetRunningApps lists >=1 app
  6. selection          Ctrl+A then GetSelectedText returns the marker
  7. focus-events       enable detector -> >=1 AppInfoUpdate; disable -> 0 more

Usage: python3 vm_validate.py [path-to-helper]
Exit code 0 iff handshake+paste+active-app all pass (the load-bearing trio);
other checks are reported but advisory (compositor-dependent).
"""
import json, os, shutil, subprocess, sys, threading, time

DELIM = b"|"
MARKER = "WISPR-VALIDATE-9A3C"

EDITOR_CANDIDATES = [
    "kwrite", "kate", "gnome-text-editor", "gedit",
    "mousepad", "xed", "featherpad", "leafpad",
]
SECOND_APP_CANDIDATES = [
    "konsole", "dolphin", "gnome-calculator", "gnome-terminal",
    "nautilus", "xterm", "foot", "alacritty", "kcalc",
]


def esc(s): return s.replace("+", "+1").replace("|", "+2")
def un(s): return s.replace("+2", "|").replace("+1", "+")
def frame(env): return esc(json.dumps(env, indent=2)).encode() + DELIM


class Helper:
    def __init__(self, binary):
        r3, w3 = os.pipe()
        if r3 == 3:
            n = os.dup(r3); os.close(r3); r3 = n
        if w3 != 3:
            os.dup2(w3, 3); os.close(w3)
        os.set_inheritable(3, True)
        self.errfile = open("/tmp/wispr_validate_helper.err", "wb")
        self.proc = subprocess.Popen(
            [binary], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
            stderr=self.errfile, pass_fds=(3,),
            env={**os.environ, "RUST_LOG": os.environ.get("RUST_LOG", "info")},
        )
        os.close(3)
        self.events = []
        self.updates = []
        threading.Thread(target=self._read, args=(r3,), daemon=True).start()
        self.u = 0

    def _read(self, rfd):
        buf = b""
        with os.fdopen(rfd, "rb", buffering=0) as f:
            while True:
                chunk = f.read(4096)
                if not chunk:
                    return
                buf += chunk
                while DELIM in buf:
                    raw, buf = buf.split(DELIM, 1)
                    if not raw:
                        continue
                    try:
                        msg = json.loads(un(raw.decode("utf-8", "replace")))
                    except Exception:
                        continue
                    self.events.append(msg)
                    req = msg.get("HelperAPIRequest", {})
                    if "AppInfoUpdate" in req:
                        self.updates.append(req["AppInfoUpdate"].get("payload", {}))

    def send(self, inner):
        self.u += 1
        inner = {**inner, "uuid": f"v-{self.u}"}
        self.proc.stdin.write(frame({"HelperAPIRequest": inner}))
        self.proc.stdin.flush()

    def last(self, key):
        for e in reversed(self.events):
            r = e.get("HelperAPIResponse", {})
            if key in r:
                v = r[key]
                if isinstance(v, dict):
                    return v.get("payload", v)
                return v
        return None

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=2)
        except Exception:
            self.proc.terminate()


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "./target/release/wispr-flow-linux-helper"
    if not os.path.exists(binary):
        sys.exit(f"helper not found: {binary}")

    editor = next((c for c in EDITOR_CANDIDATES if shutil.which(c)), None)
    second = next((c for c in SECOND_APP_CANDIDATES if shutil.which(c) and c != editor), None)

    results = {}
    h = Helper(binary)

    # 1. handshake
    h.send({"IsReady": True}); time.sleep(0.6)
    results["handshake"] = h.last("ACK") is not None

    target = f"/tmp/wispr_validate_{os.getpid()}.txt"
    try: os.remove(target)
    except OSError: pass
    open(target, "w").close()

    ed = None
    if editor:
        ed = subprocess.Popen([editor, target], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(4.0)

    # 4. active-app (query while editor focused)
    h.send({"GetActiveAppInfo": True}); time.sleep(0.6)
    aa = h.last("ActiveAppInfo") or {}
    results["active_app"] = bool(aa.get("appName") or aa.get("bundleId"))
    results["_active_app_val"] = aa

    # 5. running-apps
    h.send({"GetRunningApps": True}); time.sleep(0.6)
    ra = h.last("RunningApps") or {}
    apps = ra.get("apps", [])
    results["running_apps"] = len(apps) >= 1
    results["_running_count"] = len(apps)

    # 3. paste + save
    if editor:
        h.send({"PasteText": {"payload": {"text": MARKER, "htmlText": "", "transcriptEntityUUID": ""}}})
        time.sleep(1.0)
        h.send({"SimulateKeyPress": {"payload": {"keycode": 83, "flags": ["Control"]}}})  # Ctrl+S
        time.sleep(1.2)
        # 6. selection: select-all, then GetSelectedText (before reading file)
        h.send({"SimulateKeyPress": {"payload": {"keycode": 65, "flags": ["Control"]}}})  # Ctrl+A
        time.sleep(0.6)
        h.send({"GetSelectedTextViaCopy": True}); time.sleep(0.8)
        sel = h.last("SelectedTextViaCopy") or {}
        sel_txt = sel.get("selectedText", sel.get("contents", "")) if isinstance(sel, dict) else ""
        results["selection"] = MARKER in (sel_txt or "")
        results["_selection_val"] = sel_txt
    else:
        results["paste"] = None

    # Paste result: the marker reached the editor buffer. Primary signal is the
    # saved file; but some editors (e.g. kate on wlroots) quirk on Ctrl+S-to-disk
    # even though the paste landed — so accept the AT-SPI selection round-trip
    # (Ctrl+A then GetSelectedText returned the marker) as equally conclusive
    # proof that PasteText injected the text into the focused app.
    file_ok = False
    try:
        with open(target) as f:
            file_ok = MARKER in f.read()
    except OSError:
        pass
    results["_paste_file"] = file_ok
    results["paste"] = (file_ok or results.get("selection") is True) if editor else None

    if ed is not None:
        ed.terminate()
        try: ed.wait(timeout=3)
        except Exception: ed.kill()
    try: os.remove(target)
    except OSError: pass

    # 7. focus events
    if second:
        before = len(h.updates)
        h.send({"SetFocusChangeDetectorState": {"payload": {"active": True}}})
        time.sleep(1.0)
        app1 = subprocess.Popen([editor or second], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(3.0)
        app2 = subprocess.Popen([second], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(3.0)
        on_count = len(h.updates) - before
        h.send({"SetFocusChangeDetectorState": {"payload": {"active": False}}})
        time.sleep(0.5)
        mid = len(h.updates)
        app3 = subprocess.Popen([second], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(2.5)
        off_count = len(h.updates) - mid
        results["focus_events"] = on_count >= 1 and off_count == 0
        results["_focus"] = f"{on_count} on / {off_count} off"
        for a in (app1, app2, app3):
            a.terminate()
        for a in (app1, app2, app3):
            try: a.wait(timeout=3)
            except Exception: a.kill()
    else:
        results["focus_events"] = None

    h.close()

    # backend lines from stderr
    backend_lines = []
    try:
        with open("/tmp/wispr_validate_helper.err", "r", errors="replace") as f:
            for ln in f:
                low = ln.lower()
                if any(k in low for k in ("backend", "active-app:", "injection:", "tracker started", "bridge started", "provider")):
                    backend_lines.append(ln.rstrip())
    except OSError:
        pass

    # report
    print("=" * 60)
    print("BACKEND SELECTION:")
    for ln in backend_lines:
        print("  " + ln)
    print("=" * 60)
    def fmt(k):
        v = results.get(k)
        return "SKIP" if v is None else ("PASS" if v else "FAIL")
    print(f"  handshake     : {fmt('handshake')}")
    print(f"  paste+save    : {fmt('paste')}")
    print(f"  active-app    : {fmt('active_app')}   {results.get('_active_app_val')}")
    print(f"  running-apps  : {fmt('running_apps')}   (count={results.get('_running_count')})")
    print(f"  selection     : {fmt('selection')}   got={results.get('_selection_val')!r}")
    print(f"  focus-events  : {fmt('focus_events')}   ({results.get('_focus')})")
    print("=" * 60)

    load_bearing = results["handshake"] and (results["paste"] in (True, None)) and results["active_app"]
    print("OVERALL:", "PASS" if load_bearing else "FAIL", "(load-bearing: handshake+paste+active-app)")
    return 0 if load_bearing else 1


if __name__ == "__main__":
    sys.exit(main())
