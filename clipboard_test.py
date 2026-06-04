#!/usr/bin/env python3
"""Verify the in-process Wayland clipboard owner offers text/plain + text/html.

Drives PasteText{text, htmlText} and, while the helper is still alive (the
in-process clipboard source lives only as long as the process), inspects the
clipboard via wl-paste: list types, read text/plain, read text/html.

Usage: python3 clipboard_test.py [path-to-helper]
"""
import json, os, subprocess, sys, threading, time

DELIM = b"|"
def escape(s): return s.replace("+", "+1").replace("|", "+2")
def frame(env): return escape(json.dumps(env, indent=2)).encode() + DELIM

PLAIN = "WISPR-PLAIN-7C1"
HTML = "<b>WISPR-HTML-7C1</b>"

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
    # we don't need fd3 output here; just drain it
    threading.Thread(target=lambda: os.read(r3, 65536) and None, daemon=True).start()

    proc = subprocess.Popen(
        [binary], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=None,
        pass_fds=(3,), env={**os.environ, "RUST_LOG": os.environ.get("RUST_LOG", "warn")},
    )
    os.close(3)

    def send(inner):
        proc.stdin.write(frame({"HelperAPIRequest": {**inner, "uuid": "c-1"}})); proc.stdin.flush()

    send({"IsReady": True})
    time.sleep(0.4)
    print(f"-> PasteText(text={PLAIN!r}, htmlText={HTML!r})")
    send({"PasteText": {"payload": {"text": PLAIN, "htmlText": HTML, "transcriptEntityUUID": ""}}})
    time.sleep(0.5)

    def wlp(args):
        r = subprocess.run(["wl-paste", *args], capture_output=True)
        return r.stdout.decode("utf-8", "replace").strip(), r.returncode

    types, _ = wlp(["--list-types"])
    plain, _ = wlp(["-n"])
    html, hrc = wlp(["--type", "text/html", "-n"])
    print(f"\n[clipboard types]\n{types}")
    print(f"[text/plain ] {plain!r}")
    print(f"[text/html  ] {html!r}")

    proc.stdin.close()
    try: proc.wait(timeout=2)
    except Exception: proc.terminate()

    ok_plain = plain == PLAIN
    ok_html = "text/html" in types and html == HTML
    print()
    print("✅ PASS — clipboard offers both text/plain and text/html with correct content."
          if ok_plain and ok_html else
          f"⚠️ plain_ok={ok_plain} html_ok={ok_html} — check output above.")
    return 0 if (ok_plain and ok_html) else 1

if __name__ == "__main__":
    sys.exit(main())
