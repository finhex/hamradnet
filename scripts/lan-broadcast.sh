#!/usr/bin/env bash
# Sends games' LAN broadcasts through HamRadNet on Linux.
#
#   sudo ./scripts/lan-broadcast.sh up          Warcraft III (UDP 6112)
#   sudo ./scripts/lan-broadcast.sh up 6112 4000 27015-27020   any ports
#   sudo ./scripts/lan-broadcast.sh down        back to normal
#
# Why this is needed: many games, Warcraft III among them, look for LAN games by
# sending to the all-networks broadcast address 255.255.255.255. Despite the
# name, Linux sends such a packet out of one interface only, the one holding the
# default route - your real network card - so it never enters the tunnel and
# your friends' games never hear it. Traffic to an address in 10.66.x.x is
# routed into the tunnel normally; only this discovery step is affected.
#
# This adds a routing rule: UDP broadcasts to the listed ports go into hamrad0
# instead. Everything else that broadcasts (KDE Connect, Steam, Syncthing...)
# keeps using your real network. Run it after HamRadNet has started; the rule
# is forgotten at reboot, or removed with `down`.
#
# The rule's priority, 6000, is ahead of other tunnels' rules commonly set up
# for the same purpose (such as one for ZeroTier at 6112), so while it is in
# place HamRadNet gets the broadcasts, and `down` hands them back.
set -euo pipefail

DEV="${HAMRADNET_INTERFACE:-hamrad0}"
PRIORITY=6000
TABLE=6000

need_root() {
  if [[ $EUID -ne 0 ]]; then
    echo "Run this with sudo: it changes the routing rules." >&2
    exit 1
  fi
}

remove_rules() {
  while ip rule del priority "$PRIORITY" 2> /dev/null; do :; done
  ip route flush table "$TABLE" 2> /dev/null || true
}

case "${1:-}" in
  up)
    need_root
    shift
    ports=("${@:-6112}")
    if ! ip link show "$DEV" > /dev/null 2>&1; then
      echo "$DEV does not exist. Start HamRadNet first, then run this." >&2
      exit 1
    fi
    remove_rules
    ip route replace 255.255.255.255/32 dev "$DEV" table "$TABLE"
    for port in "${ports[@]}"; do
      ip rule add priority "$PRIORITY" to 255.255.255.255/32 ipproto udp dport "$port" lookup "$TABLE"
    done
    echo "UDP broadcasts to port(s) ${ports[*]} now go through $DEV."
    ;;
  down)
    need_root
    remove_rules
    echo "Broadcasts use your normal network again."
    ;;
  *)
    sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
    ;;
esac
