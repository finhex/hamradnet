#!/usr/bin/env bash
# Builds the Go version of every HamRadNet program, for Linux and Windows, into
# dist/go/.
#
# Go compiles its runtime into each binary, so there is nothing to install on
# any machine: one file per program, and they run as they are.
#
# The GUI is the exception to "no C toolchain needed": it draws with OpenGL, so
# the Windows build needs mingw (pacman -S mingw-w64-gcc / apt install mingw-w64).
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/common.sh
DIST=dist/go
LDFLAGS="-s -w -X main.version=0.3.0-go"
GLIBC=2.28

rm -rf "$DIST"
mkdir -p "$DIST/linux" "$DIST/windows"

echo "==> Go, Linux: server and command line (static, no dependencies at all)"
( cd go
  CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -ldflags="$LDFLAGS" \
    -o ../$DIST/linux/hamradnet-server ./cmd/hamradnet-server
  CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -ldflags="$LDFLAGS" \
    -o ../$DIST/linux/hamradnet ./cmd/hamradnet
)

echo "==> Go, Linux: the app"
# The GUI links against the system's graphics libraries, so unlike the other two
# it cannot be fully static, and a normal build only runs on systems whose glibc
# is at least as new as this one's. Zig compiles and links it against an old
# glibc instead, so it runs on any distro from 2018 on.
#
# It is built for X11 only (XWayland on a Wayland desktop), which run-gui.sh
# chooses anyway. GLFW's Wayland half is compiled against this machine's
# Wayland headers and needs libwayland 1.20, which older distros lack; it also
# pulls in the GTK decoration plugin that crashes when the app runs as root.
GUI_TAGS=x11
if command -v zig > /dev/null; then
  echo "    for glibc $GLIBC and newer, with zig"
  # Zig brings its own C library but not the X11 and OpenGL ones, which are
  # taken from this system wherever its distro keeps them.
  libdirs=""
  for dir in /usr/lib/x86_64-linux-gnu /usr/lib64 /usr/lib; do
    [[ -d "$dir" ]] && libdirs+=" -L$dir"
  done
  ( cd go
    CGO_ENABLED=1 GOOS=linux GOARCH=amd64 \
      CC="zig cc -target x86_64-linux-gnu.$GLIBC" CXX="zig c++ -target x86_64-linux-gnu.$GLIBC" \
      CGO_CFLAGS="-O2 -idirafter /usr/include" \
      CGO_LDFLAGS="$libdirs -Wl,--allow-shlib-undefined" \
      go build -tags "$GUI_TAGS" -trimpath -ldflags="$LDFLAGS" \
        -o ../$DIST/linux/hamradnet-gui ./cmd/hamradnet-gui
  )
else
  echo "!! zig not found: the app will need glibc $(ldd --version | awk 'NR==1{print $NF}') or newer." >&2
  echo "   Install zig (ziglang.org) for an app that runs on older distros too." >&2
  ( cd go
    # C17, so new compilers do not pull in glibc 2.38's C23 functions.
    CGO_ENABLED=1 GOOS=linux GOARCH=amd64 CGO_CFLAGS="-O2 -std=gnu17" \
      go build -tags "$GUI_TAGS" -trimpath -ldflags="$LDFLAGS" \
        -o ../$DIST/linux/hamradnet-gui ./cmd/hamradnet-gui
  )
fi
install -m 0755 scripts/run-gui.sh "$DIST/linux/run-gui.sh"

echo "==> Go, Windows: server and command line"
( cd go
  CGO_ENABLED=0 GOOS=windows GOARCH=amd64 go build -trimpath -ldflags="$LDFLAGS" \
    -o ../$DIST/windows/hamradnet-server.exe ./cmd/hamradnet-server
  CGO_ENABLED=0 GOOS=windows GOARCH=amd64 go build -trimpath -ldflags="$LDFLAGS" \
    -o ../$DIST/windows/hamradnet.exe ./cmd/hamradnet
)

echo "==> Go, Windows: the app"
if command -v x86_64-w64-mingw32-gcc > /dev/null; then
  ( cd go
    # -H windowsgui keeps a console window from appearing behind the app.
    CGO_ENABLED=1 GOOS=windows GOARCH=amd64 \
      CC=x86_64-w64-mingw32-gcc CXX=x86_64-w64-mingw32-g++ \
      go build -trimpath -ldflags="$LDFLAGS -H windowsgui" \
      -o ../$DIST/windows/hamradnet-gui.exe ./cmd/hamradnet-gui
  )
else
  echo "!! mingw not found, skipping the Windows app" >&2
fi

fetch_wintun "$DIST/windows"
package_windows "$DIST" hamradnet-go-windows
list_built "$DIST"
