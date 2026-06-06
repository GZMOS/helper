#!/usr/bin/env bash
# Run ON a target VM (over SSH, in a tty session). Discovers the active
# graphical session's environment, ensures /dev/uinput is writable (installs the
# packaging udev rule if needed), then runs vm_validate.py against the helper.
#
# Assumes /tmp/wispr-flow-linux-helper and /tmp/vm_validate.py are already copied
# in. Prints a concise report; exits 0 iff the load-bearing checks pass.
#
# Usage: bash vm_setup_and_validate.sh
set -uo pipefail

UID_NUM="$(id -u)"
RT="/run/user/${UID_NUM}"
export XDG_RUNTIME_DIR="$RT"
export DBUS_SESSION_BUS_ADDRESS="unix:path=${RT}/bus"

echo "### session discovery"
loginctl list-sessions --no-legend 2>/dev/null || true

# --- graphical session env ---
# The authoritative source for WAYLAND_DISPLAY/DISPLAY is the systemd *user*
# manager's environment (populated by the session at login). This is correct on
# BOTH KDE and GNOME — unlike reading a compositor process's /proc/environ, which
# breaks on GNOME where gnome-shell IS the compositor and has an empty
# WAYLAND_DISPLAY in its own environ. We still fall back to a process scan.
getuserenv() { systemctl --user show-environment 2>/dev/null | sed -n "s/^$1=//p" | head -1; }

WAYLAND_DISPLAY="$(getuserenv WAYLAND_DISPLAY)"
DISPLAY_V="$(getuserenv DISPLAY)"
XDG_CURRENT_DESKTOP="$(getuserenv XDG_CURRENT_DESKTOP)"
XDG_SESSION_TYPE="$(getuserenv XDG_SESSION_TYPE)"

# Fallback: scan a real Wayland/X11 *client* process (NOT the compositor) whose
# environ carries a display.
if [ -z "$WAYLAND_DISPLAY" ] && [ -z "$DISPLAY_V" ]; then
  for pid in $(pgrep -u "$UID_NUM" 2>/dev/null); do
    comm="$(cat /proc/$pid/comm 2>/dev/null)"
    case "$comm" in gnome-shell|kwin_wayland|sway|Hyprland|niri|mutter|weston|labwc|cosmic-comp) continue;; esac
    e="$(tr '\0' '\n' < /proc/$pid/environ 2>/dev/null)"
    if echo "$e" | grep -qE '^(WAYLAND_DISPLAY|DISPLAY)='; then
      WAYLAND_DISPLAY="$(echo "$e" | sed -n 's/^WAYLAND_DISPLAY=//p' | head -1)"
      DISPLAY_V="$(echo "$e" | sed -n 's/^DISPLAY=//p' | head -1)"
      [ -z "$XDG_CURRENT_DESKTOP" ] && XDG_CURRENT_DESKTOP="$(echo "$e" | sed -n 's/^XDG_CURRENT_DESKTOP=//p' | head -1)"
      [ -z "$XDG_SESSION_TYPE" ] && XDG_SESSION_TYPE="$(echo "$e" | sed -n 's/^XDG_SESSION_TYPE=//p' | head -1)"
      echo "### env via client-process scan (pid $pid $comm)"
      break
    fi
  done
fi

# Last-resort default for a Wayland session socket if everything above is blank.
if [ -z "$WAYLAND_DISPLAY" ] && [ -z "$DISPLAY_V" ]; then
  [ -S "${RT}/wayland-0" ] && WAYLAND_DISPLAY="wayland-0"
fi

# Only export WAYLAND_DISPLAY when non-empty: the helper's detect() uses
# var_os("WAYLAND_DISPLAY").is_some(), which is true even for an empty value,
# so exporting an empty WAYLAND_DISPLAY on a pure-X11 host wrongly selects the
# Wayland backend. Unset it otherwise to let the X11/XTEST path be chosen.
if [ -n "$WAYLAND_DISPLAY" ]; then
  export WAYLAND_DISPLAY
else
  unset WAYLAND_DISPLAY
fi
export DISPLAY="${DISPLAY_V}" XDG_CURRENT_DESKTOP XDG_SESSION_TYPE
echo "  WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-} DISPLAY=$DISPLAY"
echo "  XDG_CURRENT_DESKTOP=$XDG_CURRENT_DESKTOP XDG_SESSION_TYPE=$XDG_SESSION_TYPE"

# --- ensure /dev/uinput is writable (Wayland injection needs it) ---
echo "### uinput access"
if [ -w /dev/uinput ]; then
  echo "  /dev/uinput already writable"
else
  echo "  /dev/uinput not writable; applying access (needs sudo)"
  # Packaging rule for persistence: BOTH a logind uaccess tag AND a group/mode
  # fallback. uaccess works on some distros (Fedora) but NOT others (Arch:
  # uinput is a seatless static node, so logind never applies the ACL) — the
  # GROUP=input,MODE=0660 fallback covers those, provided the user is in `input`.
  RULE='KERNEL=="uinput", SUBSYSTEM=="misc", OPTIONS+="static_node=uinput", TAG+="uaccess", GROUP="input", MODE="0660"'
  echo "$RULE" | sudo tee /etc/udev/rules.d/99-wispr-uinput.rules >/dev/null 2>&1
  sudo udevadm control --reload-rules >/dev/null 2>&1
  sudo udevadm trigger --action=add /dev/uinput >/dev/null 2>&1
  sudo udevadm settle >/dev/null 2>&1
  sleep 0.5
  # For the *current* (already-running) session the udev rule may not retro-apply
  # — most reliable immediate grant is a direct ACL on the live node via setfacl
  # (no group membership / relogin needed). This is the test-time guarantee; the
  # rule above is what a real package ships for persistence across reboots.
  if [ ! -w /dev/uinput ]; then
    sudo setfacl -m "u:$(id -un):rw" /dev/uinput 2>/dev/null
  fi
  if [ ! -w /dev/uinput ]; then
    sudo chgrp input /dev/uinput 2>/dev/null; sudo chmod 660 /dev/uinput 2>/dev/null
  fi
  if [ -w /dev/uinput ]; then
    echo "  /dev/uinput now writable"
  else
    echo "  WARNING: still not writable; injection will fail. State:"
    getfacl /dev/uinput 2>/dev/null | grep -E 'user:|group:' || ls -l /dev/uinput
  fi
fi

echo "### validation"
cd /tmp || exit 1
python3 vm_validate.py /tmp/wispr-flow-linux-helper
rc=$?
echo "VALIDATE_RC=$rc"
exit $rc
