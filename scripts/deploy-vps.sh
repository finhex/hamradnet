#!/usr/bin/env bash
# Sets up the HamRadNet coordinator on a server, start to finish: a brand-new
# VPS with nothing on it, or one already running either version.
#
#   ./scripts/deploy-vps.sh 203.0.113.45                   the Go server (default)
#   ./scripts/deploy-vps.sh --rust 203.0.113.45            the Rust server instead
#   ./scripts/deploy-vps.sh 203.0.113.45 ubuntu            log in as ubuntu, use sudo
#   ./scripts/deploy-vps.sh --ssh-port 2222 203.0.113.45   SSH on another port
#
# It asks for the password once, then finds out the server's CPU (x86-64 or
# ARM) and builds the matching binary, uploads it, creates a service user,
# installs and starts a systemd service, opens the ports in whichever firewall
# the server runs, and finally tests TCP and UDP from here.
#
# What the server needs: a Linux with systemd (Debian, Ubuntu, Fedora, Rocky,
# Alma, Arch...), SSH, and either root or a user allowed to sudo. Nothing has to
# be installed: the binary is statically linked, so the distribution and its
# version do not matter.
#
# Both servers speak the same protocol and read the same networks.json, so you
# can switch between them by re-running this with the other flag; every client,
# Go or Rust, keeps working, and the networks survive the switch.
#
# Extra ssh options, such as a key file, go in HAMRADNET_SSH_OPTS:
#   HAMRADNET_SSH_OPTS="-i ~/.ssh/vps_key" ./scripts/deploy-vps.sh 203.0.113.45
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: ./scripts/deploy-vps.sh [--go|--rust] [--ssh-port N] <server-ip> [user]

Examples:
  ./scripts/deploy-vps.sh 203.0.113.45             installs the Go server
  ./scripts/deploy-vps.sh --rust 203.0.113.45      installs the Rust server
  ./scripts/deploy-vps.sh 203.0.113.45 ubuntu      logs in as ubuntu, uses sudo

You need the server's IP address and its root password (or a sudo user's).
Nothing else.
EOF
}

IMPL=go
SSH_PORT=22
while [[ $# -gt 0 ]]; do
  case "$1" in
    --go) IMPL=go; shift ;;
    --rust) IMPL=rust; shift ;;
    --ssh-port)
      [[ $# -ge 2 ]] || { usage; exit 1; }
      SSH_PORT="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "Unknown option $1" >&2; usage; exit 1 ;;
    *) break ;;
  esac
done
HOST="${1:-}"
SSH_USER="${2:-root}"
[[ -n "$HOST" ]] || { usage; exit 1; }

cd "$(dirname "$0")/.."

UNIT="deploy/hamradnet-server.service"
CONTROL_PORT=7777
UDP_PORT=7778

command -v ssh > /dev/null || { echo "ssh is not installed here." >&2; exit 1; }

# ------------------------------------------------------------ connection ----
# One SSH connection, reused by every later command, so the password is only
# ever typed once.
CONTROL_SOCKET="$(mktemp -u /tmp/hamradnet-ssh-XXXXXX)"
SSH_ERRORS="$(mktemp)"
read -r -a EXTRA_SSH_OPTS <<< "${HAMRADNET_SSH_OPTS:-}"
SSH_OPTS=(
  -p "$SSH_PORT"
  -o ControlMaster=auto
  -o "ControlPath=$CONTROL_SOCKET"
  -o ControlPersist=600
  -o StrictHostKeyChecking=accept-new
  -o ConnectTimeout=15
  "${EXTRA_SSH_OPTS[@]}"
)
remote() { ssh "${SSH_OPTS[@]}" "$SSH_USER@$HOST" "$@"; }
cleanup() {
  ssh "${SSH_OPTS[@]}" -O exit "$SSH_USER@$HOST" 2> /dev/null || true
  rm -f "$CONTROL_SOCKET" "$SSH_ERRORS"
}
trap cleanup EXIT

echo "==> Connecting to $SSH_USER@$HOST (enter the password once)"
if ! FACTS="$(remote 'uname -m; id -u' 2> "$SSH_ERRORS")"; then
  if grep -q 'REMOTE HOST IDENTIFICATION HAS CHANGED' "$SSH_ERRORS"; then
    known="$HOST"
    [[ "$SSH_PORT" == 22 ]] || known="[$HOST]:$SSH_PORT"
    cat >&2 <<EOF
!! This server's identity key is not the one this computer remembers for $HOST.

That is expected if you reinstalled the server, or the provider gave the IP to
a new machine. In that case, forget the old key and run this again:

    ssh-keygen -R '$known'

If you did not reinstall anything, stop here: something may be intercepting
the connection.
EOF
  else
    sed 's/^/    /' "$SSH_ERRORS" >&2
    echo "Could not connect to $SSH_USER@$HOST on port $SSH_PORT." >&2
    if grep -q 'Permission denied (publickey' "$SSH_ERRORS"; then
      echo "The server only accepts SSH keys. Pass yours like this:" >&2
      echo "    HAMRADNET_SSH_OPTS=\"-i ~/.ssh/your_key\" $0 $HOST $SSH_USER" >&2
    fi
  fi
  exit 1
fi
{ read -r ARCH; read -r REMOTE_UID; } <<< "$FACTS"

case "$ARCH" in
  x86_64|amd64) GOARCH=amd64; RUST_TARGET=x86_64-unknown-linux-musl; PLATFORM=linux ;;
  aarch64|arm64) GOARCH=arm64; RUST_TARGET=aarch64-unknown-linux-musl; PLATFORM=linux-arm64 ;;
  *)
    echo "!! The server's CPU is $ARCH. HamRadNet builds for x86-64 and 64-bit ARM only." >&2
    exit 1 ;;
esac
echo "    server CPU is $ARCH"

# ---------------------------------------------------------------- build ----
# Always rebuilt, so re-running this really does upgrade the server. Only the
# server is built; the full build-$IMPL.sh also cross-compiles the apps, which
# the VPS does not need.
BINARY="dist/$IMPL/$PLATFORM/hamradnet-server"
mkdir -p "$(dirname "$BINARY")"
echo "==> Building the $IMPL coordinator for $ARCH (static, runs on any Linux)"
if [[ "$IMPL" == go ]]; then
  ( cd go && CGO_ENABLED=0 GOOS=linux GOARCH="$GOARCH" go build -trimpath \
      -ldflags="-s -w -X main.version=0.2.0-go" \
      -o "../$BINARY" ./cmd/hamradnet-server )
else
  if ! rustup target list --installed 2>/dev/null | grep -qx "$RUST_TARGET"; then
    echo "    adding the $RUST_TARGET target"
    rustup target add "$RUST_TARGET"
  fi
  # Rust's own linker can link a static musl binary for ARM, so no ARM C
  # cross-compiler has to be installed for this.
  if [[ "$RUST_TARGET" == aarch64-* ]]; then
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
  fi
  ( cd rust && cargo build --release --target "$RUST_TARGET" -p hamradnet-server )
  cp "rust/target/$RUST_TARGET/release/hamradnet-server" "$BINARY"
fi
echo "    $BINARY ($(du -h "$BINARY" | cut -f1))"

# --------------------------------------------------------------- upload ----
# Through the SSH connection itself rather than scp, which on current OpenSSH
# needs an SFTP server on the far side that minimal images may not have.
echo "==> Uploading"
STAGE="$(remote 'mktemp -d /tmp/hamradnet.XXXXXX')"
remote "cat > $STAGE/hamradnet-server" < "$BINARY"
remote "cat > $STAGE/hamradnet-server.service" < "$UNIT"
remote "cat > $STAGE/install.sh" <<'REMOTE'
set -euo pipefail
STAGE="$1" CONTROL_PORT="$2" UDP_PORT="$3"
trap 'rm -rf "$STAGE"' EXIT

if [[ -r /etc/os-release ]]; then
  . /etc/os-release
  echo "    server runs ${PRETTY_NAME:-$ID}"
fi

if ! command -v systemctl > /dev/null || [[ ! -d /run/systemd/system ]]; then
  echo "!! This server does not run systemd, which keeps the coordinator running" >&2
  echo "   and restarts it at boot. Use a Debian, Ubuntu, Fedora, Rocky, Alma or" >&2
  echo "   Arch server instead. Nothing was changed." >&2
  exit 1
fi

# Service account with no shell and no home worth having.
if ! id hamradnet &> /dev/null; then
  useradd --system --home-dir /var/lib/hamradnet --create-home \
          --shell "$(command -v nologin || echo /usr/sbin/nologin)" hamradnet
  echo "    created the hamradnet service user"
fi
install -d -o hamradnet -g hamradnet -m 0750 /var/lib/hamradnet

# Stop before replacing the binary, or the running process holds the old inode.
systemctl stop hamradnet-server 2> /dev/null || true
install -m 0755 -o root -g root "$STAGE/hamradnet-server" /usr/local/bin/hamradnet-server
install -m 0644 -o root -g root "$STAGE/hamradnet-server.service" \
        /etc/systemd/system/hamradnet-server.service

systemctl daemon-reload
systemctl enable hamradnet-server > /dev/null 2>&1
systemctl restart hamradnet-server
echo "    installed $(/usr/local/bin/hamradnet-server --version), service started"

# Open the ports on whichever firewall is actually in use.
opened=""
if command -v ufw > /dev/null && ufw status 2> /dev/null | grep -q "Status: active"; then
  ufw allow "$CONTROL_PORT"/tcp > /dev/null
  ufw allow "$UDP_PORT"/udp > /dev/null
  opened="ufw"
elif command -v firewall-cmd > /dev/null && firewall-cmd --state &> /dev/null; then
  firewall-cmd --permanent --add-port="$CONTROL_PORT"/tcp > /dev/null
  firewall-cmd --permanent --add-port="$UDP_PORT"/udp > /dev/null
  firewall-cmd --reload > /dev/null
  opened="firewalld"
elif command -v iptables > /dev/null &&
     iptables -S INPUT 2> /dev/null | grep -qE '^-P INPUT DROP|-j (REJECT|DROP)'; then
  # Images that filter with plain iptables, Oracle Cloud's among them, end the
  # INPUT chain with a catch-all REJECT. The rules go in ahead of it.
  for rule in "-p tcp --dport $CONTROL_PORT" "-p udp --dport $UDP_PORT"; do
    # shellcheck disable=SC2086
    iptables -C INPUT $rule -j ACCEPT 2> /dev/null || iptables -I INPUT $rule -j ACCEPT
  done
  if command -v netfilter-persistent > /dev/null && netfilter-persistent save &> /dev/null; then
    opened="iptables, saved for reboots"
  elif [[ -f /etc/iptables/rules.v4 ]]; then
    iptables-save > /etc/iptables/rules.v4
    opened="iptables, saved for reboots"
  elif [[ -f /etc/sysconfig/iptables ]]; then
    iptables-save > /etc/sysconfig/iptables
    opened="iptables, saved for reboots"
  else
    opened="iptables (not saved: after a reboot, run this script again)"
  fi
elif command -v nft > /dev/null && nft list ruleset 2> /dev/null | grep -q 'hook input .*policy drop'; then
  echo "!! nftables drops incoming traffic here, and its rules are your own; allow" >&2
  echo "   TCP $CONTROL_PORT and UDP $UDP_PORT in /etc/nftables.conf." >&2
fi
if [[ -n "$opened" ]]; then
  echo "    opened $CONTROL_PORT/tcp and $UDP_PORT/udp in $opened"
else
  echo "    no host firewall blocking the ports"
fi

# Confirm it is really up, rather than trusting that systemd said so.
sleep 1
if ! systemctl is-active --quiet hamradnet-server; then
  echo "!! The service failed to start. Recent log:" >&2
  journalctl -u hamradnet-server --no-pager --lines=20 >&2
  exit 1
fi

listening=""
for _ in $(seq 1 10); do
  if command -v ss > /dev/null; then
    listening=$(ss -lntu 2> /dev/null | grep -E ":($CONTROL_PORT|$UDP_PORT)\b" || true)
  fi
  [[ -n "$listening" ]] && break
  sleep 0.5
done
if [[ -n "$listening" ]]; then
  echo "    listening:"
  echo "$listening" | awk '{print "      " $1 "  " $5}'
elif ! command -v ss > /dev/null; then
  echo "    (ss not installed; skipping the listening check)"
else
  echo "!! Service is running but nothing is listening on $CONTROL_PORT/$UDP_PORT." >&2
  exit 1
fi
REMOTE

# ---------------------------------------------------------------- setup ----
echo "==> Installing"
INSTALL=(bash "$STAGE/install.sh" "$STAGE" "$CONTROL_PORT" "$UDP_PORT")
if [[ "$REMOTE_UID" == 0 ]]; then
  remote "${INSTALL[@]}"
elif remote sudo -n true 2> /dev/null; then
  remote sudo -n "${INSTALL[@]}"
else
  # sudo wants a password; -t gives it a terminal to ask on.
  echo "    sudo on the server will ask for $SSH_USER's password"
  ssh -t "${SSH_OPTS[@]}" "$SSH_USER@$HOST" sudo "${INSTALL[@]}"
fi

# ------------------------------------------------------------- verify ------
# Either command line can check either server; use whichever has been built.
echo "==> Checking the server from here"
CHECKER=""
for candidate in "dist/$IMPL/linux/hamradnet" dist/go/linux/hamradnet dist/rust/linux/hamradnet; do
  [[ -x "$candidate" ]] && CHECKER="$candidate" && break
done
if [[ -n "$CHECKER" ]]; then
  if ! "$CHECKER" check --server "$HOST:$CONTROL_PORT"; then
    echo "    The service is running, but the check above failed from here."
    echo "    Look at your provider's firewall or security group and allow"
    echo "    TCP $CONTROL_PORT and UDP $UDP_PORT."
  fi
elif command -v nc > /dev/null && nc -z -w 5 "$HOST" "$CONTROL_PORT" 2> /dev/null; then
  echo "    $HOST:$CONTROL_PORT is reachable (build a client to also test UDP)"
else
  echo "    could not check from here; allow TCP $CONTROL_PORT and UDP $UDP_PORT"
  echo "    in your provider's firewall if clients cannot connect."
fi

SSH_HINT="ssh $SSH_USER@$HOST"
[[ "$SSH_PORT" == 22 ]] || SSH_HINT="ssh -p $SSH_PORT $SSH_USER@$HOST"
[[ "$REMOTE_UID" == 0 ]] || SSH_HINT="$SSH_HINT sudo"

cat <<EOF

────────────────────────────────────────────────────────────────
 $IMPL coordinator is live at $HOST:$CONTROL_PORT
────────────────────────────────────────────────────────────────

Any HamRadNet app works with it, Go or Rust. In the app, enter the
server address $HOST, then create a network and send its ID and
password to your friends.

Server management:
  $SSH_HINT systemctl status hamradnet-server
  $SSH_HINT journalctl -u hamradnet-server -f

Re-run this script any time to upgrade the server to a new build, or
with --go / --rust to switch which server runs.
EOF
