# Shared by build-go.sh and build-rust.sh. Sourced, not run.

WINTUN_VERSION=0.14.1

# The Windows client loads wintun.dll at runtime for the network adapter; it is
# not linked in, and is the one file that has to travel beside the exe.
fetch_wintun() {
  local dest="$1" scratch
  echo "==> Fetching wintun.dll"
  if ! command -v curl > /dev/null || ! command -v unzip > /dev/null; then
    echo "    curl or unzip missing; get wintun.dll from https://www.wintun.net" >&2
    return 0
  fi
  scratch="$(mktemp -d)"
  if curl -fsSL --retry 2 "https://www.wintun.net/builds/wintun-${WINTUN_VERSION}.zip" \
       -o "$scratch/wintun.zip" 2> /dev/null; then
    unzip -qo "$scratch/wintun.zip" -d "$scratch"
    cp "$scratch/wintun/bin/amd64/wintun.dll" "$dest/"
    echo "    bundled wintun.dll ${WINTUN_VERSION}"
  else
    echo "    could not download wintun.dll; get it from https://www.wintun.net" >&2
  fi
  rm -rf "$scratch"
}

# One zip to hand to a friend, so the exe and the DLL cannot be separated. The
# folder inside is named after the zip, so the Go and Rust builds unpack side
# by side instead of on top of each other.
package_windows() {
  local dist="$1" name="$2" scratch
  command -v zip > /dev/null || { echo "    zip missing; not packaging" >&2; return 0; }
  scratch="$(mktemp -d)"
  cp -r "$dist/windows" "$scratch/$name"
  rm -f "$dist/$name.zip"
  ( cd "$scratch" && zip -qr "$name.zip" "$name" )
  mv "$scratch/$name.zip" "$dist/"
  rm -rf "$scratch"
  echo "==> Packaged $dist/$name.zip"
}

# The Linux app comes two ways, made from the same programs:
#   linux-standalone/  started with run-gui.sh, which asks for the password
#                      each time
#   linux-service/     set up once with install.sh as a background service,
#                      after which the app runs from the menu with no password
# dist/*/linux/ keeps every Linux program, the server too, for the scripts.
package_linux() {
  local dist="$1" variant
  for variant in linux-standalone linux-service; do
    rm -rf "${dist:?}/$variant"
    mkdir -p "$dist/$variant"
    cp "$dist/linux/hamradnet" "$dist/linux/hamradnet-gui" "$dist/$variant/"
  done
  install -m 0755 scripts/run-gui.sh "$dist/linux-standalone/run-gui.sh"
  install -m 0755 scripts/install-service.sh "$dist/linux-service/install.sh"
}

list_built() {
  echo
  echo "Built:"
  find "$1" -type f -exec ls -lh {} \; | awk '{print "  " $9 "  " $5}' | sort
}
