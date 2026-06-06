#!/usr/bin/env python3
"""Live focus-event test (any compositor with an active-app provider).

Enables the focus-change detector (SetFocusChangeDetectorState{active:true}) and
verifies the helper emits AppInfoUpdate requests on fd 3:
  * once immediately for the current focus, then
  * once per window activation as focus moves.

It launches a couple of GUI apps to move focus around (auto-detected from what's
installed, so it runs on KDE / GNOME / wlroots alike). Run inside a live
graphical session. Usage: python3 focus_test.py [path-to-helper]
"""
import json, os, shutil, subprocess, sys, threading, time

# Candidate GUI apps to bounce focus between; we pick the first two distinct ones
# that exist. Mix toolkits so it works across desktops.
APP_CANDIDATES = [
    "kwrite", "kate", "konsole", "dolphin",            # KDE / Qt
    "gnome-text-editor", "gedit", "gnome-calculator",  # GNOME / GTK
    "gnome-terminal", "nautilus", "mousepad", "xterm", "foot", "alacritty",
]

DELIM = b"|"
def escape(s): return s.replace("+", "+1").replace("|", "+2")
def unescape(s): return s.replace("+2", "|").replace("+1", "+")
def frame(env): return escape(json.dumps(env, indent=2)).encode() + DELIM

events = []
def reader_thread(rfd):
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
                    msg = json.loads(unescape(raw.decode("utf-8", "replace")))
                    events.append(msg)
                    req = msg.get("HelperAPIRequest", {})
                    if "AppInfoUpdate" in req:
                        p = req["AppInfoUpdate"].get("payload", {})
                        print(f"  [fd3] AppInfoUpdate: appName={p.get('appName')!r} "
                              f"bundleId={p.get('bundleId')!r} title={p.get('windowTitle')!r}")
                except Exception:
                    pass


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "./target/release/wispr-flow-linux-helper"
    if not os.path.exists(binary):
        sys.exit(f"helper not found: {binary}")

    r3, w3 = os.pipe()
    if r3 == 3:
        r3n = os.dup(r3); os.close(r3); r3 = r3n
    if w3 != 3:
        os.dup2(w3, 3); os.close(w3)
    os.set_inheritable(3, True)

    proc = subprocess.Popen(
        [binary], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=None,
        pass_fds=(3,), env={**os.environ, "RUST_LOG": os.environ.get("RUST_LOG", "info")},
    )
    os.close(3)
    threading.Thread(target=reader_thread, args=(r3,), daemon=True).start()

    u = [0]
    def send(inner, label):
        u[0] += 1
        print(f"-> {label}")
        proc.stdin.write(frame({"HelperAPIRequest": {**inner, "uuid": f"t-{u[0]}"}})); proc.stdin.flush()

    chosen = []
    for cand in APP_CANDIDATES:
        if shutil.which(cand) and cand not in chosen:
            chosen.append(cand)
        if len(chosen) == 2:
            break
    if len(chosen) < 2:
        print(f"⚠️  need two GUI apps to bounce focus; found {chosen}. Install more (e.g. gedit, xterm).")
        proc.stdin.close()
        return 3
    app_a, app_b = chosen[0], chosen[1]

    send({"IsReady": True}, "IsReady")
    time.sleep(0.5)

    print("\n[enable focus detector — expect an immediate AppInfoUpdate for current focus]")
    n0 = len(events)
    send({"SetFocusChangeDetectorState": {"payload": {"active": True}}}, "SetFocusChangeDetectorState(active=true)")
    time.sleep(1.0)

    print(f"\n[launch {app_a} — expect an AppInfoUpdate as it activates]")
    apps = []
    apps.append(subprocess.Popen([app_a], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    time.sleep(3.0)
    print(f"\n[launch {app_b} — expect another AppInfoUpdate]")
    apps.append(subprocess.Popen([app_b], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    time.sleep(3.0)

    print("\n[disable focus detector — launching another app should NOT emit]")
    send({"SetFocusChangeDetectorState": {"payload": {"active": False}}}, "SetFocusChangeDetectorState(active=false)")
    time.sleep(0.5)
    before_off = len([e for e in events if "AppInfoUpdate" in e.get("HelperAPIRequest", {})])
    apps.append(subprocess.Popen([app_a], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    time.sleep(2.5)
    after_off = len([e for e in events if "AppInfoUpdate" in e.get("HelperAPIRequest", {})])

    # cleanup
    for a in apps:
        a.terminate()
    for a in apps:
        try: a.wait(timeout=3)
        except Exception: a.kill()
    proc.stdin.close()
    try: proc.wait(timeout=2)
    except Exception: proc.terminate()

    updates = [e for e in events if "AppInfoUpdate" in e.get("HelperAPIRequest", {})]
    print(f"\n[summary] {len(updates)} AppInfoUpdate events total; "
          f"{after_off - before_off} emitted while detector was OFF (expect 0).")
    ok = len(updates) >= 2 and (after_off - before_off) == 0
    print("✅ PASS — focus events stream when on and stop when off." if ok
          else "⚠️ check output above.")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
