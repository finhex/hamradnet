#!/usr/bin/env bash
# Installs the coordinator on a VPS as a systemd service.
#
# Copy the binary and this repo's deploy/ directory to the server, then:
#   sudo ./scripts/install-server.sh [path-to-hamradnet-server]
#
# Either server works, Go or Rust: they speak the same protocol and read the
# same networks.json. Without a path it takes dist/go/linux/hamradnet-server,
# then dist/rust/linux/hamradnet-server.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run this with sudo." >&2
  exit 1
fi

cd "$(dirname "$0")/.."

BINARY="${1:-}"
if [[ -z "$BINARY" ]]; then
  for candidate in dist/go/linux/hamradnet-server dist/rust/linux/hamradnet-server; do
    [[ -f "$candidate" ]] && BINARY="$candidate" && break
  done
fi
if [[ ! -f "$BINARY" ]]; then
  echo "Could not find hamradnet-server. Build it first, or pass the path." >&2
  exit 1
fi

echo "==> Installing $BINARY ($("$BINARY" --version)) to /usr/local/bin"
systemctl stop hamradnet-server 2> /dev/null || true
install -m 0755 "$BINARY" /usr/local/bin/hamradnet-server

if ! id hamradnet &> /dev/null; then
  echo "==> Creating the hamradnet service user"
  useradd --system --home /var/lib/hamradnet --create-home --shell /usr/sbin/nologin hamradnet
fi
install -d -o hamradnet -g hamradnet -m 0750 /var/lib/hamradnet

echo "==> Installing the systemd unit"
install -m 0644 deploy/hamradnet-server.service /etc/systemd/system/hamradnet-server.service
systemctl daemon-reload
systemctl enable hamradnet-server
systemctl restart hamradnet-server

echo
systemctl --no-pager --lines=5 status hamradnet-server || true

cat <<'EOF'

Done. Open these ports in your VPS firewall if it has one:

  TCP 7777   control plane
  UDP 7778   relay and endpoint reflection

  ufw:      sudo ufw allow 7777/tcp && sudo ufw allow 7778/udp
  firewalld: sudo firewall-cmd --permanent --add-port=7777/tcp \
             && sudo firewall-cmd --permanent --add-port=7778/udp \
             && sudo firewall-cmd --reload

Logs:   journalctl -u hamradnet-server -f
EOF
