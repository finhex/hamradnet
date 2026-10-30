#!/usr/bin/env bash
# Proves the Go and Rust programs work with each other, with real traffic.
#
# Each server (Go, Rust) is run against each pairing of clients (Go+Go,
# Rust+Rust, Go+Rust, Rust+Go), once with the clients able to reach each other
# directly and once with them forced through the server's relay. Every run
# starts the real binaries, creates a network on the first client, joins it from
# the second, then sends pings and a few megabytes of TCP across the tunnel in
# both directions, and finally runs a Warcraft III-style LAN search to
# 255.255.255.255, which must find the other player with no setup at all.
#
# The first client to connect gets the lower peer ID, and the lower ID starts
# the encrypted handshake, so Go+Rust and Rust+Go between them cover each
# language on each side of it.
#
# It needs no root and changes nothing on this machine. Everything runs inside a
# throwaway user and network namespace: the kernel feature containers are built
# on, used directly, with no container runtime. Each client gets a network
# namespace and tunnel interface of its own, linked to the server's by a virtual
# cable, so traffic really crosses the tunnel instead of looping back.
#
#   ./scripts/interop-test.sh             tests dist/go/linux and dist/rust/linux
#   GO_BIN=dir RUST_BIN=dir ./scripts/interop-test.sh
#   ONLY="go rust go *" ./scripts/interop-test.sh     one server and pairing
set -euo pipefail

cd "$(dirname "$0")/.."
GO_BIN="${GO_BIN:-$PWD/dist/go/linux}"
RUST_BIN="${RUST_BIN:-$PWD/dist/rust/linux}"

if [[ "${HAMRADNET_INTEROP_INNER:-}" != 1 ]]; then
  for dir in "$GO_BIN" "$RUST_BIN"; do
    for exe in hamradnet hamradnet-server; do
      if [[ ! -x "$dir/$exe" ]]; then
        echo "Missing $dir/$exe - run ./scripts/build-all.sh first." >&2
        exit 1
      fi
    done
  done
  exec unshare --user --map-root-user --net \
    env HAMRADNET_INTEROP_INNER=1 GO_BIN="$GO_BIN" RUST_BIN="$RUST_BIN" "$0" "$@"
fi

# ------------------------------------------------------------ the network ----
#
#   client A ns          server ns (this one)           client B ns
#   eth0 10.99.1.2 ──── va 10.99.1.1   vb 10.99.2.1 ──── eth0 10.99.2.2
#
# Both clients use 10.99.1.1 as the server. Turning IP forwarding on in the
# middle lets A and B reach each other directly; turning it off leaves the
# server's relay as their only way through.

WORK="$(mktemp -d)"
KEEP_WORK=0
BACKGROUND=()
cleanup() {
  for pid in "${BACKGROUND[@]}"; do kill "$pid" 2> /dev/null || true; done
  wait 2> /dev/null || true
  if [[ $KEEP_WORK == 1 ]]; then
    echo "Logs kept in $WORK"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

ip link set lo up

# `unshare` only switches namespace once it is running, so wait for that before
# moving an interface into it, or the interface lands in this namespace instead.
new_netns() {
  unshare --net sleep infinity &
  local pid=$!
  BACKGROUND+=("$pid")
  until [[ "$(readlink "/proc/$pid/ns/net")" != "$(readlink /proc/self/ns/net)" ]]; do
    sleep 0.05
  done
  NEW_NS=$pid
}
in_ns() { local pid=$1; shift; nsenter --target "$pid" --net -- "$@"; }

new_netns; NS_A=$NEW_NS
new_netns; NS_B=$NEW_NS
ip link add va type veth peer name eth0 netns "$NS_A"
ip link add vb type veth peer name eth0 netns "$NS_B"
ip addr add 10.99.1.1/24 dev va && ip link set va up
ip addr add 10.99.2.1/24 dev vb && ip link set vb up
for side in "$NS_A 10.99.1" "$NS_B 10.99.2"; do
  read -r ns net <<< "$side"
  in_ns "$ns" ip link set lo up
  in_ns "$ns" ip addr add "$net.2/24" dev eth0
  in_ns "$ns" ip link set eth0 up
  in_ns "$ns" ip route add default via "$net.1"
done

SERVER_ADDR=10.99.1.1:7777

# ------------------------------------------------------------- one case ----

bin_for() { [[ $1 == go ]] && echo "$GO_BIN" || echo "$RUST_BIN"; }
other() { [[ $1 == go ]] && echo rust || echo go; }

# cli <ns> <impl> <name> <args...>: run that implementation's command line
# against the named client, whichever implementation the client itself is.
cli() {
  local ns=$1 bin; bin="$(bin_for "$2")"; local dir="$RUN/$3"; shift 3
  HAMRADNET_CONFIG="$dir/config.json" HAMRADNET_IPC="$dir/ipc.json" \
    in_ns "$ns" timeout 20 "$bin/hamradnet" "$@"
}

start_client() {
  local ns=$1 bin; bin="$(bin_for "$2")"; local dir="$RUN/$3"
  mkdir -p "$dir"
  HAMRADNET_CONFIG="$dir/config.json" HAMRADNET_IPC="$dir/ipc.json" \
    in_ns "$ns" "$bin/hamradnet" up --server "$SERVER_ADDR" --name "$3" \
    > "$dir/daemon.log" 2>&1 &
  CASE_PIDS+=($!)
}

# Waits until the client reports itself connected, and prints its address.
wait_connected() {
  local status=""
  for _ in $(seq 1 60); do
    status="$(cli "$@" status 2> /dev/null || true)"
    if grep -q '(connected)' <<< "$status"; then
      sed -n 's/^address *: *//p' <<< "$status"
      return 0
    fi
    sleep 0.25
  done
  return 1
}

# Receives a TCP stream on the given address and prints how many bytes came.
SINK='import socket, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind((sys.argv[1], 5001)); s.listen(1); s.settimeout(30)
c, _ = s.accept(); c.settimeout(30); n = 0
while True:
    d = c.recv(65536)
    if not d: break
    n += len(d)
print(n)'
# Sends that many bytes, and prints the throughput in MB/s.
SOURCE='import socket, sys, time
total = int(sys.argv[2]); chunk = b"x" * 65536
for _ in range(50):
    try:
        c = socket.create_connection((sys.argv[1], 5001), timeout=10); break
    except OSError:
        time.sleep(0.1)
started = time.time(); sent = 0
while sent < total:
    sent += c.send(chunk[: total - sent])
c.shutdown(socket.SHUT_WR); c.recv(1); c.close()
print("%.1f" % (total / (time.time() - started) / 1e6))'
TRANSFER_BYTES=$((4 * 1024 * 1024))

# transfer <from ns> <to ns> <to address>: prints MB/s, or fails.
transfer() {
  local got
  in_ns "$2" python3 -I -c "$SINK" "$3" > "$RUN/sink.out" 2>&1 &
  local sink=$!
  local rate
  rate="$(in_ns "$1" timeout 40 python3 -I -c "$SOURCE" "$3" "$TRANSFER_BYTES" 2>> "$RUN/sink.out")" || {
    kill "$sink" 2> /dev/null; return 1; }
  wait "$sink" || return 1
  got="$(tail -1 "$RUN/sink.out")"
  [[ "$got" == "$TRANSFER_BYTES" ]] || return 1
  echo "$rate"
}

# A game's LAN search, the way Warcraft III does it: from port 6112 to
# 255.255.255.255:6112, waiting for the host's answer. That broadcast leaves
# through eth0, not the tunnel, so it only reaches B if the client copies it in.
SEARCH='import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
s.bind(("0.0.0.0", 6112)); s.settimeout(5)
s.sendto(b"searchgame", ("255.255.255.255", 6112))
try:
    while True:
        data, src = s.recvfrom(1500)
        if data == b"gameinfo":
            print(src[0]); break
except socket.timeout:
    print("no answer")'
# A game hosting: answers the first search it hears, straight back to the sender.
HOST='import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", 6112)); s.settimeout(8)
try:
    while True:
        data, src = s.recvfrom(1500)
        if data == b"searchgame":
            s.sendto(b"gameinfo", src); break
except socket.timeout:
    pass'

# discover <searcher ns> <host ns> <host address>: succeeds if the search found
# the host through the tunnel.
discover() {
  in_ns "$2" python3 -I -c "$HOST" &
  local host=$!
  sleep 0.3
  local answer
  answer="$(in_ns "$1" python3 -I -c "$SEARCH")"
  wait "$host" 2> /dev/null || true
  [[ "$answer" == "$3" ]]
}

FAILED=0
RESULTS=()

# run_case <server impl> <client A impl> <client B impl> <direct|relay>
run_case() {
  local server=$1 a=$2 b=$3 path=$4
  RUN="$WORK/${server}-server_${a}-${b}_${path}"
  mkdir -p "$RUN"
  CASE_PIDS=()
  local result="" detail=""

  if [[ $path == direct ]]; then echo 1; else echo 0; fi > /proc/sys/net/ipv4/ip_forward

  "$(bin_for "$server")/hamradnet-server" --bind 0.0.0.0 --no-store --empty-grace 0 \
    > "$RUN/server.log" 2>&1 &
  CASE_PIDS+=($!)
  for _ in $(seq 1 40); do
    (exec 3<> /dev/tcp/10.99.1.1/7777) 2> /dev/null && break
    sleep 0.1
  done

  local name_a="alice-$a" name_b="bob-$b" addr_a addr_b network_id path_seen
  local password="pw-$RANDOM"
  start_client "$NS_A" "$a" "$name_a"
  if ! addr_a="$(wait_connected "$NS_A" "$a" "$name_a")"; then
    result=FAIL detail="client A ($a) never connected to the $server server"
  fi

  # The network is created and joined with the *other* language's command line,
  # which proves either `hamradnet` can drive either daemon: the two share one
  # ipc.json on a real machine.
  if [[ -z $result ]]; then
    network_id="$(cli "$NS_A" "$(other "$a")" "$name_a" create "interop" --password "$password" 2>&1 \
      | tee "$RUN/create.out" | grep -oE 'HAM-[0-9]{4}-[0-9]{4}' | head -1)"
    [[ -n $network_id ]] || result=FAIL detail="creating a network failed: $(tr '\n' ' ' < "$RUN/create.out")"
  fi
  if [[ -z $result ]]; then
    start_client "$NS_B" "$b" "$name_b"
    if ! addr_b="$(wait_connected "$NS_B" "$b" "$name_b")"; then
      result=FAIL detail="client B ($b) never connected to the $server server"
    elif ! cli "$NS_B" "$(other "$b")" "$name_b" join "$network_id" --password "$password" > "$RUN/join.out" 2>&1; then
      result=FAIL detail="joining failed: $(tr '\n' ' ' < "$RUN/join.out")"
    fi
  fi

  # The first ping waits out the handshake, and in relay mode the time it takes
  # to give up on a direct path.
  if [[ -z $result ]]; then
    local up=0
    for _ in $(seq 1 45); do
      if in_ns "$NS_A" ping -c 1 -W 1 "$addr_b" > /dev/null 2>&1; then up=1; break; fi
    done
    [[ $up == 1 ]] || result=FAIL detail="A could never ping B ($addr_b) through the tunnel"
  fi

  local rtt="" loss="" rate_ab="" rate_ba=""
  if [[ -z $result ]]; then
    # Full-size packets: 1252 bytes of ICMP data fills the 1280-byte tunnel MTU.
    local ping_out
    ping_out="$(in_ns "$NS_A" ping -q -c 30 -i 0.05 -s 1252 -M do "$addr_b" 2>&1 || true)"
    loss="$(grep -oE '[0-9.]+% packet loss' <<< "$ping_out" | cut -d% -f1)"
    rtt="$(grep -oE '= [0-9.]+/[0-9.]+' <<< "$ping_out" | cut -d/ -f2)"
    if [[ -z $loss || ${loss%.*} -gt 10 ]]; then
      result=FAIL detail="full-size pings lost ${loss:-all}%"
    elif ! in_ns "$NS_B" ping -c 3 -W 2 "$addr_a" > /dev/null 2>&1; then
      result=FAIL detail="B could not ping A ($addr_a): the tunnel only works one way"
    elif ! rate_ab="$(transfer "$NS_A" "$NS_B" "$addr_b")"; then
      result=FAIL detail="TCP from A to B did not arrive intact"
    elif ! rate_ba="$(transfer "$NS_B" "$NS_A" "$addr_a")"; then
      result=FAIL detail="TCP from B to A did not arrive intact"
    elif ! discover "$NS_A" "$NS_B" "$addr_b"; then
      result=FAIL detail="a game's LAN search (255.255.255.255:6112) from A never found B"
    fi
  fi

  if [[ -z $result ]]; then
    # A link may come up through the relay first, when the relay answers the
    # handshake before the direct path does, and switch to direct once the
    # client's next probe gets through. Probes go out when the link has been
    # quiet for a few seconds, so give that time rather than looking once.
    for _ in $(seq 1 40); do
      cli "$NS_A" "$a" "$name_a" peers > "$RUN/peers.out" 2>&1 || true
      path_seen="$(grep -F "$name_b" "$RUN/peers.out" | grep -oE '\b(direct|relay)\b' | head -1)"
      [[ "$path_seen" == "$path" ]] && break
      sleep 0.5
    done
    cli "$NS_A" "$(other "$a")" "$name_a" status > "$RUN/status-other.out" 2>&1 || true
    cli "$NS_A" "$(other "$a")" "$name_a" peers > "$RUN/peers-other.out" 2>&1 || true
    if [[ "$path_seen" != "$path" ]]; then
      result=FAIL detail="traffic flowed, but A reports the path as '${path_seen:-unknown}', expected $path"
    elif ! grep -qE "^address *: *$addr_a\$" "$RUN/status-other.out"; then
      result=FAIL detail="the $(other "$a") CLI could not read A's status: $(tr '\n' ' ' < "$RUN/status-other.out")"
    elif ! grep -qF "$name_b" "$RUN/peers-other.out"; then
      result=FAIL detail="the $(other "$a") CLI could not list A's peers: $(tr '\n' ' ' < "$RUN/peers-other.out")"
    else
      result=PASS
      detail="ping ${rtt} ms, ${loss}% loss; TCP A→B ${rate_ab} MB/s, B→A ${rate_ba} MB/s; LAN search found B"
    fi
  fi

  for pid in "${CASE_PIDS[@]}"; do kill "$pid" 2> /dev/null || true; done
  for pid in "${CASE_PIDS[@]}"; do wait "$pid" 2> /dev/null || true; done

  printf '  %-4s  %-4s %-4s  %-6s  %s  %s\n' "$server" "$a" "$b" "$path" "$result" "$detail"
  if [[ $result != PASS ]]; then
    FAILED=$((FAILED + 1))
    KEEP_WORK=1
    for log in "$RUN/server.log" "$RUN/$name_a/daemon.log" "$RUN/$name_b/daemon.log"; do
      [[ -f $log ]] || continue
      echo "        --- last lines of ${log#"$RUN"/}"
      tail -n 6 "$log" | sed 's/^/        /'
    done
  fi
}

echo "Go:   $GO_BIN ($("$GO_BIN/hamradnet-server" --version))"
echo "Rust: $RUST_BIN ($("$RUST_BIN/hamradnet-server" --version))"
echo
echo "  server client A+B  path    result"
RAN=0
for server in go rust; do
  for pair in "go go" "rust rust" "go rust" "rust go"; do
    for path in direct relay; do
      # ONLY="go rust go direct" runs just that combination; wildcards work.
      # shellcheck disable=SC2053
      [[ -z "${ONLY:-}" || "$server $pair $path" == $ONLY ]] || continue
      RAN=$((RAN + 1))
      # shellcheck disable=SC2086
      run_case "$server" $pair "$path"
    done
  done
done

echo
if [[ $FAILED == 0 ]]; then
  echo "All $RAN combinations work."
else
  echo "$FAILED of $RAN combinations failed."
  exit 1
fi
