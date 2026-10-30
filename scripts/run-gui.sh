#!/usr/bin/env bash
# Starts the HamRadNet app with the privileges AND the display it needs. Works
# for both the Go and the Rust app; build-go.sh and build-rust.sh each put a
# copy next to their app.
#
# `sudo hamradnet-gui` does not work: sudo drops DISPLAY, so the window cannot
# be opened. This passes it through explicitly.
#
# Wayland is deliberately hidden, so the app draws through X11 and XWayland. On
# native Wayland the Go app's toolkit (GLFW) loads libdecor, whose GTK plugin
# loads the user's icon theme through a sandboxed image loader that aborts when
# run as root, killing the app before its window appears.
#
# Hiding it takes both variables. GLFW ignores a missing WAYLAND_DISPLAY and
# tries the default socket under XDG_RUNTIME_DIR anyway, so that goes too; the
# Rust app's toolkit (winit) gives up instead of falling back if WAYLAND_DISPLAY
# names a socket that is not there, so it is unset rather than faked.
set -euo pipefail

cd "$(dirname "$0")"
BINARY="${1:-}"
if [[ -z "$BINARY" ]]; then
  for candidate in ./hamradnet-gui ../dist/go/linux/hamradnet-gui ../dist/rust/linux/hamradnet-gui; do
    [[ -x "$candidate" ]] && BINARY="$candidate" && break
  done
fi
if [[ ! -x "$BINARY" ]]; then
  echo "Could not find hamradnet-gui. Pass its path: ./run-gui.sh /path/to/hamradnet-gui" >&2
  exit 1
fi

X11_ONLY=(-u WAYLAND_DISPLAY -u XDG_RUNTIME_DIR XDG_SESSION_TYPE=x11)

if [[ $EUID -eq 0 ]]; then
  exec env "${X11_ONLY[@]}" "$BINARY"
fi

echo "Starting HamRadNet (it needs root to create the network interface)..."
exec sudo -E env "${X11_ONLY[@]}" \
  DISPLAY="${DISPLAY:-:0}" \
  XAUTHORITY="${XAUTHORITY:-$HOME/.Xauthority}" \
  "$BINARY"
