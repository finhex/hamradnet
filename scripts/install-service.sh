#!/usr/bin/env bash
# Installs HamRadNet as a background service, so the app starts from your app
# menu as your normal user, with no password to type.
#
#   sudo ./install.sh                install, or upgrade, or switch Go <-> Rust
#   sudo ./install.sh --uninstall    remove it all again
#   sudo ./install.sh --uninstall --purge   ...and forget your identity and networks
#
# Once installed, "Uninstall HamRadNet" in the app menu, or the command
# `hamradnet-uninstall`, removes it without needing this folder.
#
# What it installs:
#   /usr/local/bin/hamradnet, hamradnet-gui, hamradnet-uninstall
#   /etc/systemd/system/hamradnet.service   the tunnel, started at boot
#   two entries in the app menu: HamRadNet, and Uninstall HamRadNet
#   a firewall rule letting your HamRadNet friends' traffic in (ufw/firewalld)
#
# Your identity, server and saved networks live in /etc/hamradnet. They are kept
# unless you uninstall with --purge, so reinstalling, or switching between the
# Go and Rust versions, picks up where you left off.
set -euo pipefail

BIN=/usr/local/bin
UNIT=/etc/systemd/system/hamradnet.service
APPS=/usr/local/share/applications
CONFIG_DIR=/etc/hamradnet
INTERFACE=hamrad0

mode=install
purge=0
yes=0
# Run as `hamradnet-uninstall`, this same script does the removing.
[[ "$(basename "$0")" == hamradnet-uninstall ]] && mode=uninstall
for arg in "$@"; do
  case "$arg" in
    --uninstall) mode=uninstall ;;
    --purge) purge=1 ;;
    --yes|-y) yes=1 ;;
    -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "Unknown option $arg" >&2; exit 2 ;;
  esac
done

# Ask for root here rather than making people remember sudo. pkexec covers
# systems without sudo, and the menu's uninstall entry.
if [[ $EUID -ne 0 ]]; then
  if command -v sudo > /dev/null; then
    exec sudo "$0" "$@"
  elif command -v pkexec > /dev/null; then
    exec pkexec "$(realpath "$0")" "$@"
  fi
  echo "This needs root. Run it with sudo." >&2
  exit 1
fi

if ! command -v systemctl > /dev/null || [[ ! -d /run/systemd/system ]]; then
  echo "This system does not run systemd, which the service needs." >&2
  echo "Use the standalone version instead, started with run-gui.sh." >&2
  exit 1
fi

# The firewall rule: friends in your network may reach you on the tunnel, as on
# a home LAN, which is what games need. Without it a firewall that drops
# incoming traffic, as ufw does by default, lets you see games but not join
# them, or not see them at all.
firewall_open() {
  if command -v ufw > /dev/null && ufw status 2> /dev/null | grep -q "Status: active"; then
    ufw allow in on "$INTERFACE" comment "HamRadNet: traffic from your HamRadNet networks" > /dev/null
    echo "    ufw: allowed traffic coming in through $INTERFACE"
  elif command -v firewall-cmd > /dev/null && firewall-cmd --state &> /dev/null; then
    firewall-cmd --quiet --permanent --zone=trusted --add-interface="$INTERFACE"
    firewall-cmd --quiet --reload
    echo "    firewalld: put $INTERFACE in the trusted zone"
  fi
}

firewall_close() {
  if command -v ufw > /dev/null && ufw status 2> /dev/null | grep -q "Status: active"; then
    ufw delete allow in on "$INTERFACE" > /dev/null 2>&1 || true
  fi
  if command -v firewall-cmd > /dev/null && firewall-cmd --state &> /dev/null; then
    firewall-cmd --quiet --permanent --zone=trusted --remove-interface="$INTERFACE" 2> /dev/null || true
    firewall-cmd --quiet --reload 2> /dev/null || true
  fi
}

refresh_menus() {
  command -v update-desktop-database > /dev/null && update-desktop-database -q "$APPS" 2> /dev/null || true
}

# ------------------------------------------------------------- uninstall ----
if [[ $mode == uninstall ]]; then
  if [[ $yes == 0 && -t 0 ]]; then
    what="the HamRadNet service, programs and menu entries"
    [[ $purge == 1 ]] && what+=", and your identity and saved networks"
    read -r -p "Remove $what? [y/N] " answer
    [[ "$answer" == [yY]* ]] || { echo "Nothing removed."; exit 0; }
  fi
  echo "==> Removing HamRadNet"
  systemctl disable --now hamradnet.service > /dev/null 2>&1 || true
  rm -f "$UNIT"
  systemctl daemon-reload
  echo "    stopped and removed the service"
  rm -f "$BIN/hamradnet" "$BIN/hamradnet-gui" "$BIN/hamradnet-uninstall"
  rm -f "$APPS/hamradnet.desktop" "$APPS/hamradnet-uninstall.desktop"
  refresh_menus
  echo "    removed the programs and menu entries"
  firewall_close
  rm -f "$CONFIG_DIR/ipc.json"
  if [[ $purge == 1 ]]; then
    rm -rf "$CONFIG_DIR"
    echo "    forgot your identity and saved networks"
  elif [[ -d "$CONFIG_DIR" ]]; then
    echo "    kept $CONFIG_DIR, your identity and saved networks; --purge removes it"
  fi
  echo "HamRadNet is uninstalled."
  if [[ -t 0 && "$(basename "$0")" == hamradnet-uninstall ]]; then
    read -r -p "Press Enter to close." _
  fi
  exit 0
fi

# --------------------------------------------------------------- install ----
HERE="$(cd "$(dirname "$0")" && pwd)"
for program in hamradnet hamradnet-gui; do
  if [[ ! -x "$HERE/$program" ]]; then
    echo "$program is missing from $HERE; run this from the unpacked HamRadNet folder." >&2
    exit 1
  fi
done

# The service runs as root, but the app runs as whoever installed it, and only
# that user is let in to control it.
user="${SUDO_USER:-}"
if [[ -z "$user" && -n "${PKEXEC_UID:-}" ]]; then
  user="$(getent passwd "$PKEXEC_UID" | cut -d: -f1)"
fi
if [[ -z "$user" || "$user" == root ]]; then
  echo "Run this with sudo from your own account, so the app can be set up for you." >&2
  exit 1
fi

echo "==> Installing HamRadNet $("$HERE/hamradnet" version 2> /dev/null || "$HERE/hamradnet" --version) for $user"

# An upgrade stops the old service first. Anything still holding the tunnel
# after that is another HamRadNet, such as the standalone app, and the
# service could not create its own.
systemctl stop hamradnet.service 2> /dev/null || true
sleep 1
if ip link show "$INTERFACE" > /dev/null 2>&1; then
  echo "Another HamRadNet is running ($INTERFACE exists). Close the HamRadNet app," >&2
  echo "or stop whatever started it, then run this again." >&2
  exit 1
fi

install -m 0755 "$HERE/hamradnet" "$BIN/hamradnet"
install -m 0755 "$HERE/hamradnet-gui" "$BIN/hamradnet-gui"
install -m 0755 "$0" "$BIN/hamradnet-uninstall"
echo "    installed the programs in $BIN"

cat > "$UNIT" <<EOF
# Installed by HamRadNet's install.sh; removed by hamradnet-uninstall.
[Unit]
Description=HamRadNet virtual LAN
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
# Root, to create the tunnel interface. $user's app and command line control it.
ExecStart=$BIN/hamradnet up --ipc-user $user
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF

mkdir -p "$APPS"
cat > "$APPS/hamradnet.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=HamRadNet
GenericName=Virtual LAN for games
Comment=Play LAN games with friends over the internet
Exec=$BIN/hamradnet-gui
Icon=network-vpn
Terminal=false
Categories=Network;
Keywords=LAN;VPN;games;Hamachi;Radmin;
EOF
cat > "$APPS/hamradnet-uninstall.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Uninstall HamRadNet
Comment=Remove the HamRadNet service, app and menu entries
Exec=$BIN/hamradnet-uninstall
Icon=edit-delete
Terminal=true
Categories=Network;
EOF
refresh_menus
echo "    added HamRadNet and Uninstall HamRadNet to the app menu"

firewall_open

# The control file names the port of the service that wrote it; one left by
# the service being replaced would send the check below to a closed port.
rm -f "$CONFIG_DIR/ipc.json"
systemctl daemon-reload
systemctl enable --now hamradnet.service > /dev/null 2>&1
echo "    started the service; it also starts at every boot"

# Confirm the user can really drive it, rather than trusting that it started.
reachable=0
for _ in $(seq 1 20); do
  if runuser -u "$user" -- "$BIN/hamradnet" status > /dev/null 2>&1; then
    reachable=1
    break
  fi
  sleep 0.5
done
if [[ $reachable == 0 ]]; then
  echo "!! The service started, but $user cannot reach it yet. Its log:" >&2
  journalctl -u hamradnet.service --no-pager --lines=15 >&2
  exit 1
fi

cat <<EOF

────────────────────────────────────────────────────────────────
 HamRadNet is installed
────────────────────────────────────────────────────────────────

Open HamRadNet from your app menu: no password needed any more.
The first time, enter your server's address and your name.

To remove it: "Uninstall HamRadNet" in the app menu, or run
  hamradnet-uninstall
EOF
