#!/usr/bin/env bash
# Builds the Rust version of every HamRadNet program, for Linux and Windows,
# into dist/rust/.
#
# The Windows build needs the mingw toolchain:
#   rustup target add x86_64-pc-windows-gnu
#   # Debian/Ubuntu: apt install mingw-w64
#   # Arch:          pacman -S mingw-w64-gcc
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/common.sh
DIST=dist/rust
TARGET_DIR=rust/target
LINUX_TARGET=x86_64-unknown-linux-musl
WINDOWS_TARGET=x86_64-pc-windows-gnu
GLIBC=2.28

rm -rf "$DIST"
mkdir -p "$DIST/linux" "$DIST/windows"

for target in "$LINUX_TARGET" "$WINDOWS_TARGET"; do
  if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
    echo "    adding the $target target"
    rustup target add "$target"
  fi
done

# Static, so the binaries run on any Linux regardless of its glibc. A default
# build links against the build machine's glibc and then refuses to start on an
# older distro, which is exactly the situation on a typical VPS.
echo "==> Rust, Linux: server and command line (static)"
( cd rust && cargo build --release --target "$LINUX_TARGET" -p hamradnet -p hamradnet-server )
cp "$TARGET_DIR/$LINUX_TARGET/release/hamradnet" "$DIST/linux/"
cp "$TARGET_DIR/$LINUX_TARGET/release/hamradnet-server" "$DIST/linux/"

# The GUI links against system graphics libraries, so it is built against glibc
# rather than musl - a static binary cannot dlopen the GPU drivers it needs. A
# normal build then only runs on systems whose glibc is at least as new as this
# one's; cargo-zigbuild links it against an old glibc instead, so it runs on any
# distro from 2018 on.
echo "==> Rust, Linux: the app"
if command -v zig > /dev/null && command -v cargo-zigbuild > /dev/null; then
  echo "    for glibc $GLIBC and newer, with zig"
  ( cd rust && cargo zigbuild --release --target "x86_64-unknown-linux-gnu.$GLIBC" -p hamradnet-gui )
  cp "$TARGET_DIR/x86_64-unknown-linux-gnu/release/hamradnet-gui" "$DIST/linux/"
else
  echo "!! zig or cargo-zigbuild not found: the app will need glibc $(ldd --version | awk 'NR==1{print $NF}') or newer." >&2
  echo "   Install zig (ziglang.org) and \`cargo install cargo-zigbuild\` for one that runs on older distros." >&2
  ( cd rust && cargo build --release -p hamradnet-gui )
  cp "$TARGET_DIR/release/hamradnet-gui" "$DIST/linux/"
fi
install -m 0755 scripts/run-gui.sh "$DIST/linux/run-gui.sh"

echo "==> Rust, Windows: server, command line and the app"
( cd rust && cargo build --release --target "$WINDOWS_TARGET" \
    -p hamradnet -p hamradnet-server -p hamradnet-gui )
for exe in hamradnet hamradnet-server hamradnet-gui; do
  cp "$TARGET_DIR/$WINDOWS_TARGET/release/$exe.exe" "$DIST/windows/"
done

fetch_wintun "$DIST/windows"
package_windows "$DIST" hamradnet-rust-windows
list_built "$DIST"
