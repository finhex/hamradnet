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
LDFLAGS="-s -w -X main.version=0.2.0-go"

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
( cd go
  # The GUI links against the system's graphics libraries, so unlike the other
  # two it cannot be fully static.
  CGO_ENABLED=1 GOOS=linux GOARCH=amd64 go build -trimpath -ldflags="$LDFLAGS" \
    -o ../$DIST/linux/hamradnet-gui ./cmd/hamradnet-gui
)
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
