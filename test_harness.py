#!/usr/bin/env python3
"""Stand-in for Electron's helper host — exercises the Linux helper over the real
stdio topology (commands on stdin/fd0, events on fd3) without the full app.

It spawns the helper exactly like the bundle does (4 pipes), sends framed
HelperAPIRequest envelopes, and decodes whatever the helper writes to fd 3.

Usage:
    python3 test_harness.py [path-to-helper-binary]

Default binary path: ./target/debug/wispr-flow-linux-helper
"""
import json
import os
import subprocess
import sys
import threading
import time

DELIM = b"|"


def escape(s: str) -> str:
    # mirrors the bundle: '+' -> '+1', then '|' -> '+2'
    return s.replace("+", "+1").replace("|", "+2")


def unescape(s: str) -> str:
    return s.replace("+2", "|").replace("+1", "+")


def frame(envelope: dict) -> bytes:
    body = json.dumps(envelope, indent=2)  # JSON.stringify(_, null, 2)
    return escape(body).encode("utf-8") + DELIM


def reader_thread(rfd: int):
    """Read fd 3 (helper -> host), split on '|', unescape, pretty-print."""
    buf = b""
    with os.fdopen(rfd, "rb", buffering=0) as f:
        while True:
            chunk = f.read(4096)
            if not chunk:
                print("[fd3] EOF")
                return
            buf += chunk
            while DELIM in buf:
                raw, buf = buf.split(DELIM, 1)
                if not raw:
                    continue
                try:
                    msg = json.loads(unescape(raw.decode("utf-8", "replace")))
                    print("[fd3] <-", json.dumps(msg))
                except Exception as e:  # noqa: BLE001
                    print(f"[fd3] <- (unparsable: {e}) {raw!r}")


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "./target/debug/wispr-flow-linux-helper"
    if not os.path.exists(binary):
        sys.exit(f"helper binary not found: {binary}\nBuild it first: cargo build")

    # fd 3 pipe: child writes (-> fd 3), parent reads. The helper hard-codes fd 3,
    # so we force the pipe's write end onto fd 3 *in the parent* and pass it through
    # by number (pass_fds). This is more robust than a preexec_fn dup2, which races
    # with subprocess's own fd-closing.
    r3, w3 = os.pipe()
    # fd 3 is usually the lowest free fd, so os.pipe() often hands the READ end fd 3.
    # Relocate the read end out of the way before forcing the write end onto fd 3,
    # otherwise dup2(w3, 3) clobbers our own read end.
    if r3 == 3:
        r3_new = os.dup(r3)
        os.close(r3)
        r3 = r3_new
    if w3 != 3:
        os.dup2(w3, 3)   # clears CLOEXEC on fd 3
        os.close(w3)
    os.set_inheritable(3, True)

    proc = subprocess.Popen(
        [binary],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,   # non-IPC; helper logs a warning if it writes here
        stderr=None,              # inherit -> helper logs visible in terminal
        pass_fds=(3,),
        env={**os.environ, "RUST_LOG": os.environ.get("RUST_LOG", "info")},
    )
    os.close(3)  # parent's copy of the write end; child keeps its own
    threading.Thread(target=reader_thread, args=(r3,), daemon=True).start()

    def send(envelope: dict, label: str):
        print(f"[fd0] -> {label}: {json.dumps(envelope)}")
        proc.stdin.write(frame(envelope))
        proc.stdin.flush()

    u = 0
    def uuid():
        nonlocal u
        u += 1
        return f"test-{u}"

    # --- scripted conversation ---
    send({"HelperAPIRequest": {"IsReady": True, "uuid": uuid()}}, "IsReady")
    time.sleep(0.3)
    send({"HelperAPIRequest": {"GetActiveAppInfo": True, "uuid": uuid()}}, "GetActiveAppInfo")
    time.sleep(0.3)
    send({"HelperAPIRequest": {"GetRunningApps": True, "uuid": uuid()}}, "GetRunningApps")
    time.sleep(0.3)
    send({"HelperAPIRequest": {"GetAccessibilityStatus": True, "uuid": uuid()}}, "GetAccessibilityStatus")
    time.sleep(0.3)

    # Uncomment to exercise injection (will type into whatever window is focused!):
    # send({"HelperAPIRequest": {"PasteText": {"payload":
    #       {"text": "hello from linux helper", "htmlText": "", "transcriptEntityUUID": ""}},
    #       "uuid": uuid()}}, "PasteText")
    # time.sleep(0.3)

    print("\n(waiting 1s for any late events; Ctrl-C to stop)")
    time.sleep(1.0)
    proc.stdin.close()
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        proc.terminate()


if __name__ == "__main__":
    main()
