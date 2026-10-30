# HamRadNet

A virtual LAN for playing games with friends over the internet — the same idea as
Radmin VPN or Hamachi, but you host the server yourself.

Everyone who joins a network gets an address on a shared `10.66.0.0/16` subnet.
To a game, it looks like you are all plugged into the same switch. Traffic
between players is encrypted end to end and
goes directly peer-to-peer whenever the two networks allow it; when they don't,
it falls back to relaying through your server.

```
   your PC                    your VPS                  friend's PC
┌─────────────┐          ┌──────────────────┐         ┌─────────────┐
│  the game   │          │   coordinator    │         │  the game   │
│      │      │          │  • who is online │         │      │      │
│  hamrad0    │          │  • network list  │         │  hamrad0    │
│  10.66.0.2  │          │  • relay (UDP)   │         │  10.66.0.3  │
└──────┬──────┘          └────────┬─────────┘         └──────┬──────┘
       │                          │                          │
       │  ── control: who exists, where they are (TCP) ──    │
       ├──────────────────────────┼──────────────────────────┤
       │                          │                          │
       │      direct, encrypted, hole-punched (UDP)          │
       └─────────────────────────────────────────────────────┘
                                  ↑
                    falls back through the relay
                    if the two NATs refuse to punch
```

## Two versions: Go and Rust

Every program exists twice, once written in Go and once in Rust:

| | Go | Rust |
|---|---|---|
| Source | `go/` | `rust/` |
| Built programs | `dist/go/` | `dist/rust/` |
| Windows zip for friends | `dist/go/hamradnet-go-windows.zip` | `dist/rust/hamradnet-rust-windows.zip` |
| Linux app, as a service | `dist/go/linux-service/` | `dist/rust/linux-service/` |
| Linux app, standalone | `dist/go/linux-standalone/` | `dist/rust/linux-standalone/` |

On Linux the **service** version is the comfortable one: `sudo ./install.sh`
once, then HamRadNet starts from your app menu with no password. The
**standalone** version asks for your password every time you start it with
`run-gui.sh`. The two Linux folders hold the same app; see
[Getting it running](#getting-it-running).

Both speak the same protocol, so **mix them freely**. A Go app connects to a
Rust server, a Rust app tunnels to a Go app, and either command line controls
either app. Use whichever runs better for you. Your friends do not need the
same one as you.

`./scripts/interop-test.sh` proves this. It runs both servers against every
pairing of clients and pushes real traffic through the tunnel each time. See
[Testing](#testing).

**New here?** [GUIDE.md](GUIDE.md) explains everything step by step in plain
words: setting up a server from nothing, starting the app, and getting games
such as Warcraft III to find each other.

## What you need

* **A server with a public IP** — any cheap VPS. It coordinates who is in which
  network and relays for players who cannot connect directly.
* **Ports open on it:** TCP 7777 and UDP 7778.
* Players need **no port forwarding**. That is the point of the hole punching.

## Setting up the server

One command, from your own machine, works on a brand-new server with nothing
installed. All you need is the server's IP address and its root password:

```bash
./scripts/deploy-vps.sh 203.0.113.45                   # the Go server
./scripts/deploy-vps.sh --rust 203.0.113.45            # or the Rust server
./scripts/deploy-vps.sh 203.0.113.45 ubuntu            # no root login? a sudo user
./scripts/deploy-vps.sh --ssh-port 2222 203.0.113.45   # SSH on another port
HAMRADNET_SSH_OPTS="-i ~/.ssh/vps_key" ./scripts/deploy-vps.sh 203.0.113.45   # key login
```

It asks for the password once, then does everything. It checks whether the
server is x86-64 or ARM and builds to match, then uploads the binary. It creates
an unprivileged `hamradnet` service user, and installs and starts a systemd
service that also starts at boot. It opens the ports in whichever firewall the
server uses, including the plain iptables rules some providers ship, such as
Oracle Cloud. Last, it tests TCP and UDP from your machine. Nothing to copy by
hand, nothing to install on the server. Re-run it any time to upgrade.

The server needs a Linux with systemd: Debian, Ubuntu, Fedora, Rocky, Alma or
Arch, on x86-64 or 64-bit ARM. It was tested on a fresh Debian 13 cloud image,
logging in both as root and as a sudo user, with an iptables setup that rejects
everything except SSH, and through a reboot.

If you reinstall the server and keep its IP, SSH will notice the server's new
identity key. The script stops and tells you the one command to run.

To switch servers, re-run it with the other flag. Both servers read the same
`networks.json`, so your networks, IDs and passwords survive the switch. To see
which one is running:

```bash
ssh root@203.0.113.45 hamradnet-server --version
#   hamradnet-server 0.4.0-go   → Go (the Go version always ends in -go)
#   hamradnet-server 0.4.0      → Rust
```

**An IP address is all you need** — no domain name, no DNS, no certificates.

The binary is statically linked, so it does not care which Debian or Ubuntu
version is on there, or whether anything at all is installed. Networks are saved
to `/var/lib/hamradnet/networks.json` and survive a restart. The service is idle
most of the time; only relaying uses real bandwidth, and only for players who
cannot connect directly.

To confirm it works, from anywhere:

```bash
hamradnet check --server 203.0.113.45:7777
```

That tests both TCP and UDP — a firewall that allows one but drops the other is
the most common reason a setup half-works. It needs no root and changes nothing.

<details>
<summary>Installing on the server by hand instead</summary>

```bash
sudo ./scripts/install-server.sh          # takes dist/go, then dist/rust
sudo ./scripts/install-server.sh dist/rust/linux/hamradnet-server   # pick one
# or, fully manual (either hamradnet-server binary):
sudo cp hamradnet-server /usr/local/bin/
sudo useradd --system --home /var/lib/hamradnet --create-home hamradnet
sudo cp deploy/hamradnet-server.service /etc/systemd/system/
sudo systemctl enable --now hamradnet-server
```

Then `systemctl status hamradnet-server` and `journalctl -u hamradnet-server -f`.
</details>

### Looking after it

```bash
ssh root@203.0.113.45 systemctl status hamradnet-server   # is it up?
ssh root@203.0.113.45 journalctl -u hamradnet-server -f   # watch it live
ssh root@203.0.113.45 systemctl restart hamradnet-server  # restart it
./scripts/deploy-vps.sh 203.0.113.45                      # upgrade to a new build
```

It starts on boot, restarts itself if it ever crashes, and runs as an
unprivileged user that owns nothing but `/var/lib/hamradnet`. Upgrading is just
re-running the deploy script; your networks are in `networks.json` and survive
it.

Everyone must speak the same *protocol* version. That is version 1 in both
the Go and the Rust builds today, which is why they mix. A client refuses to
connect to a server on a different protocol version and says so, rather than
half-working. If a future change ever bumps it, hand your friends the new zip
too.

## Playing — the app

`hamradnet-gui` is the whole thing in one window: it connects, creates networks,
joins them and shows who is online. The tunnel runs inside it, so there is no
service to start and nothing to type.

### Getting it running

**Windows.** Unzip `hamradnet-go-windows.zip` or `hamradnet-rust-windows.zip`
and keep the files together: `hamradnet-gui.exe` and `wintun.dll` must sit in
the same folder. Right-click the exe and choose **Run as administrator**.
Windows only lets administrators create a network adapter, and the app tells
you so in plain words if you forget.

**Linux** needs root for the same reason, and comes two ways. The app is the
same in both.

- **Service** (`linux-service/`, recommended). Run `sudo ./install.sh` once.
  It installs a background service that holds the tunnel and starts at boot,
  adds HamRadNet to the app menu, and opens `hamrad0` in ufw or firewalld.
  After that the app runs from the menu as your normal user, with no
  password, and talks to the service over the same local channel the command
  line uses. **Uninstall HamRadNet** in the menu, or `hamradnet-uninstall`,
  removes the service, the programs, the menu entries and the firewall rule.
  `--purge` also removes your identity and networks in `/etc/hamradnet`.
- **Standalone** (`linux-standalone/`). Start it with the launcher next to it,
  which asks for your password every time:

  ```bash
  dist/go/linux/run-gui.sh       # the Go app
  dist/rust/linux/run-gui.sh     # the Rust app
  ```

  Not `sudo hamradnet-gui`. `sudo` drops the settings that let the app reach
  your screen. The launcher asks for your password (through `sudo`, or
  `pkexec` where there is no `sudo`), keeps the display, and draws the window
  through X11, or XWayland on a Wayland desktop, which looks the same. If ufw
  is on, run `sudo ufw allow in on hamrad0` once, or your friends' games
  stay invisible and yours cannot be joined.

The app always looks for a running service first, so the two never fight over
the tunnel.

The apps run on any 64-bit Linux from about 2018 on (glibc 2.28 or newer); both
were tested on Debian 11. See [What it runs on](GUIDE.md#what-it-runs-on).

### First run

It asks for two things:

* **Server address** — just the IP of your server, e.g. `203.0.113.45`. The port
  is filled in for you.
* **Your name** — what your friends see in the list.

Both are saved, so every later start goes straight to the main window and
reconnects on its own. To point it at a different server, use **Change server**
in the top right; your settings come back up ready to edit.

### Using it

**Create a network** asks for a name and a password, then gives you an ID like
`HAM-4821-9903`. Send that ID and the password to your friends — **Copy ID** puts
it on the clipboard for pasting into chat. **Join a network** takes an ID and
password somebody sent you.

You can be in several networks at once. Each gets its own panel listing exactly
who is in it, with a player count:

```
┌─ game night ──────────────────  3 players  [Copy ID] [Leave] [Delete] ─┐
│  Address     Name          Status   Ping     Traffic                   │
│  10.66.0.3   you (you)     online   74 ms    1.2 MB in · 0.9 MB out    │
│  10.66.0.2   alice         online   31 ms    600 KB in · 480 KB out    │
│  10.66.0.7   bob           online   96 ms    410 KB in · 300 KB out    │
└────────────────────────────────────────────────────────────────────────┘
```

* **Address** is what you type into a game. Yours is the row marked *(you)*.
* **Ping** is the real round trip to that player, measured inside the tunnel, so
  it is the number that matters for a game. Green below 60 ms, yellow to 150 ms,
  orange above. Hover it to see whether that player is reached directly or routed
  through your server.
* Your own **Ping** is to the *server*, not to a player. Only relayed traffic
  pays it.

No public IP addresses are shown anywhere, so the window is safe to screenshot.

**Leave** removes you from a network but leaves it standing for everyone else.
**Delete** removes it for everyone, and only its creator can.

### Sharing it with friends

Send them `dist/go/hamradnet-go-windows.zip` or
`dist/rust/hamradnet-rust-windows.zip`. Either works with your server, whichever
one you run. Tell them to unzip the whole folder and keep the files together,
then right-click `hamradnet-gui.exe` and Run as administrator. They need the
server IP, the network ID and the password. Nothing else, and no port forwarding
on their router.

## If something will not connect

**The app says it needs administrator rights.** It does; see above. Without them
it cannot create the network adapter.

**It sits on "Connecting…".** Check the server from any machine:

```bash
hamradnet check --server 203.0.113.45:7777
```

That tests TCP and UDP separately and tells you which one is blocked. A firewall
that allows one and drops the other is the usual culprit.

**A player shows "connecting" and never turns green.** Their tunnel is not coming
up. Give it a minute: it retries, and falls back to routing through your server
if a direct path cannot be made.

**Everything looks connected but the game cannot see anyone.** Test the tunnel
itself first — `ping 10.66.0.2` (their address from the list) should reply. If
the ping works and the game's LAN list stays empty, open **Log** in the app and
look for a line about LAN broadcasts; [GUIDE.md](GUIDE.md#games) explains how
games are found. If the list fills but joining fails, it is the host's firewall:
on Windows, allow the game through Windows Defender Firewall for both private
and public networks, since Windows usually files the HamRadNet adapter under
public.

**Anything else.** Click **Log** in the app. **Copy all** puts the whole log on
the clipboard, ready to paste wherever you are asking for help.

## The command line, if you prefer

Everything the app does is also available from `hamradnet`, which is what to use
on a machine with no desktop. It is `dist/go/linux/hamradnet` or
`dist/rust/linux/hamradnet` (`hamradnet.exe` on Windows); copy either to
`/usr/local/bin/` to type it as plain `hamradnet`. Either one controls a running
app or daemon of either language:

```bash
sudo hamradnet up --server 203.0.113.45:7777 --name yourname   # leave running
sudo hamradnet create "game night" --password something-secret # prints the ID
sudo hamradnet join HAM-4821-9903 --password something-secret
```

Check who is connected:

```
$ hamradnet peers
ADDRESS          NAME               LINK         PATH         PING         RX         TX
10.66.0.2        alice              up           direct      31 ms     1.2 MB     0.9 MB
10.66.0.7        bob                up           relay       96 ms     410 KB     300 KB
```

`direct` means the traffic goes straight to that player. `relay` means it is
going through your server — it still works, with your server's latency added.
The client keeps probing and upgrades a `relay` link to `direct` by itself if a
path opens up.

## What happens to a network when everyone leaves

**It survives, and is cleaned up once nobody has used it for 14 days.**

Closing the app is not the same as abandoning a network, so a network outlives
its members going offline: the same ID and password work again tomorrow, or next
weekend, without anyone resharing anything. Only when nobody has been in it for
a fortnight is it removed.

The rules, in full:

* **Fourteen days empty.** A network with nobody in it for 14 days is removed
  automatically, whether it was used and then abandoned or created and never
  joined at all. Anyone joining resets the clock, so a network in use is never
  at risk. The server takes `--empty-grace <seconds>` if you want it shorter;
  `0` removes a network the moment its last member disconnects.
* **Delete.** Whoever created a network can remove it at once, even with people
  still in it, from the app or with `hamradnet delete <id>`. Other members
  cannot, even knowing the password — the server checks it against the creator's
  identity key. Everyone in it is told, and the ID stops working immediately.

Leaving is different from deleting: you are removed and your client stops
rejoining at startup, but the network carries on existing for everyone else.

In the game, host a LAN game as usual. If a game asks for an address, give it
the one from `hamradnet peers`. If its LAN list stays empty, see
[Games](GUIDE.md#games) in the guide.

## Playing Minecraft

Everyone joins the network in the app first, then find the host's address in the
Players list — say `10.66.0.2`.

**Java Edition, quick game.** The host opens a world, presses Esc, chooses
**Open to LAN**, and clicks **Start LAN World**. The chat prints a port number,
e.g. `Local game hosted on port 54321`. Everyone else goes to **Multiplayer →
Direct Connection** and enters `10.66.0.2:54321`.

The host's world must be open *before* the others connect, and the port changes
every time it is reopened.

**Java Edition, a server that stays up.** Run the usual `server.jar` on the
host. Leave `server-ip=` blank in `server.properties` so it listens on every
interface, including the tunnel. Everyone connects to `10.66.0.2:25565` under
**Multiplayer → Add Server**, which remembers it. `online-mode=true` is fine;
this is a real internet connection, not a crack.

**Bedrock Edition.** Add a server with address `10.66.0.2` and port `19132`.

**Does "Scan for LAN games" work?** Sometimes. Minecraft announces LAN games to
the multicast address `224.0.252.28:4445`, and those packets are copied to every
peer here, so the mechanism is in place. Whether the game *sends* them down the
tunnel is up to the operating system's choice of multicast interface, which
neither the game nor this program controls. Direct Connection always works, so
use that if the list stays empty.

**If it will not connect**, it is almost always the host's firewall rather than
the tunnel. On Windows, allow Java through Windows Defender Firewall for private
networks. Check the tunnel itself first: from another player's machine,
`ping 10.66.0.2` should reply. If the ping works and the game does not, it is
the firewall or the port.

## Commands

| Command | What it does |
|---|---|
| `hamradnet up --server HOST:PORT --name NAME` | Run the daemon. Root/Administrator. Both flags may be left out, as the service does; it then waits for `connect`. `--ipc-user NAME` lets that user drive it. |
| `hamradnet connect --server HOST [--name NAME]` | Point the running daemon at a server, as the app's Connect button does. Port 7777 is assumed. |
| `hamradnet logs` | The running daemon's recent log. |
| `hamradnet create NAME --password PW` | Make a network and join it. |
| `hamradnet join ID --password PW` | Join someone's network. |
| `hamradnet leave ID` | Leave, and stop rejoining at startup. |
| `hamradnet delete ID` | Remove a network for everyone. Creator only. |
| `hamradnet status` | Connection state, your address, your networks. |
| `hamradnet peers` | Everyone reachable, and how. |
| `hamradnet check --server HOST:PORT` | Test a server's TCP and UDP. No root. |
| `hamradnet key` | This machine's public key. |

The daemon remembers the networks you joined and rejoins them on startup, so
`up` is normally all you need after the first time.

## How it works

**Control plane (TCP).** The client connects to the coordinator, which assigns it
an address and tells it who else is in its networks — their public keys and the
addresses worth trying. It pushes updates as people come and go.

**Data plane (UDP).** Every pair of players runs its own encrypted session using
the Noise `IK` pattern over X25519 + ChaCha20-Poly1305 — the same construction
WireGuard uses. The coordinator distributes public keys but never holds a session
key, so it cannot read what it relays.

**Getting a direct path.** Both sides send UDP to each other's candidate
addresses at once, which opens a mapping in each NAT so the other side's packets
have somewhere to land. Whichever address answers first wins. If none do, the
pair uses the relay and keeps probing in the background.

**Broadcast.** Games find each other on a LAN by broadcasting. A packet that
enters the tunnel addressed to a broadcast or multicast address is copied to
every peer in the network, the way a real switch floods one. The catch is
getting it into the tunnel. A broadcast to the tunnel's own subnet,
`10.66.255.255`, always goes in. A broadcast to `255.255.255.255`, which is how
Warcraft III and many other games search, goes out of a single interface chosen
by the operating system, normally the real network card. So on Linux the client
watches, with a packet socket and an in-kernel filter, for UDP broadcasts
leaving through other interfaces. It sends a copy into the tunnel from its
tunnel address, and the original still reaches the local network. A short list
of ports belonging to services that announce the computer itself (DHCP,
NetBIOS, KDE Connect, Syncthing, Steam streaming) is left out. On Windows the
client instead gives the tunnel adapter the lowest interface metric, which makes
it the one Windows sends such broadcasts through.

## Security

* Traffic between players is encrypted end to end. The server relays ciphertext
  and cannot decrypt it.
* A handshake is only answered if the key belongs to someone the coordinator says
  shares a network with you.
* The relay only forwards between peers who share a network, and a peer must
  prove its identity with a token issued over its authenticated control
  connection before anything is delivered to it.
* Replay protection uses a 64-packet sliding window, and a peer's address is only
  believed once a packet from it has been authenticated.

Worth knowing: **network passwords are the only thing keeping strangers out of a
network.** They are salted and hashed on the server, but a network ID plus its
password is all anyone needs, so pick a real password and share it privately.
Anyone in your network can reach your machine on `10.66.x.x` exactly as if they
were on your LAN — treat it like letting someone into your house's wifi.

## Building

```bash
./scripts/build-all.sh      # both: dist/go/ and dist/rust/
./scripts/build-go.sh       # just the Go one  -> dist/go/{linux,windows}
./scripts/build-rust.sh     # just the Rust one -> dist/rust/{linux,windows}
```

Each puts the Linux programs, the Windows programs with `wintun.dll`, and a
ready-to-send Windows zip under its own folder in `dist/`.

### Build tools

| Tool | What it is for | Minimum | Used for v0.4.0 |
|---|---|---|---|
| [Go](https://go.dev/dl/) | every Go program | 1.24 (`go.mod`) | 1.27.3 |
| [Rust](https://rustup.rs), via rustup | every Rust program | 1.95 (egui 0.36 needs it) | 1.96.0 |
| [Zig](https://ziglang.org/download/) | Linux apps that run on old distros | not pinned | 0.17.0 |
| [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) | the same, for the Rust app | not pinned | 0.24.0 |
| mingw-w64 GCC | the Windows builds | not pinned | 16.2.0 |
| wintun.dll | the Windows network adapter; downloaded by the build | 0.14.1 (pinned in `scripts/common.sh`) | 0.14.1 |

"Not pinned" means only the version in the last column has been tried.

Rust also needs these targets: `x86_64-unknown-linux-musl` for the static
server and command line, `x86_64-pc-windows-gnu` for Windows, and
`aarch64-unknown-linux-musl` only when `deploy-vps.sh --rust` meets an ARM
server (it adds that one itself).

**Zig is optional but recommended.** With it, both Linux apps are linked
against glibc 2.28 and run on any Linux from 2018 on. Without it, they only run
on systems at least as new as the one that built them, and the build says so.

On Arch, CachyOS or Manjaro:

```bash
sudo pacman -S go rustup zig mingw-w64-gcc
rustup default stable
rustup target add x86_64-unknown-linux-musl x86_64-pc-windows-gnu
cargo install --locked cargo-zigbuild
```

On Debian or Ubuntu, Go and Zig are best taken from their websites, as the
distro's packages are often older than the minimums above:

```bash
sudo apt install build-essential gcc-mingw-w64-x86-64 libgl1-mesa-dev xorg-dev
curl https://sh.rustup.rs -sSf | sh
rustup target add x86_64-unknown-linux-musl x86_64-pc-windows-gnu
cargo install --locked cargo-zigbuild
```

The releases are built on CachyOS. The Debian/Ubuntu package list has not been
tried for a full build.

The libraries the code depends on are pinned in `go/go.sum` and
`rust/Cargo.lock`. The main ones are Fyne 2.8.1 for the Go app, eframe/egui
0.36.2 for the Rust app, wireguard-go's `tun` package and the wintun crate
0.5.1 for the network adapter, snow 0.9.6 for Noise in Rust, and tokio 1.53.

## Testing

```bash
(cd go && go test ./...)          # the Go suite
(cd rust && cargo test)           # the Rust suite
./scripts/interop-test.sh         # Go and Rust against each other
```

`interop-test.sh` is the one that matters for mixing versions. It takes the
built programs in `dist/` and runs each server, Go and Rust, against each
pairing of clients: Go+Go, Rust+Rust, Go+Rust and Rust+Go. Each pairing runs
once with a direct path and once forced through the relay, so 16 runs in all.
Each run creates a network, joins it, then sends full-size pings and 4 MB of TCP
across the tunnel in both directions. Last, it runs a Warcraft III-style LAN
search to `255.255.255.255`, which must find the other player with no setup.
The network is created and joined with the *other* language's command line,
which proves either one controls either app. It needs no root and changes
nothing on your machine: it runs in a throwaway network namespace, the kernel
feature containers use, without Docker or any other container runtime.
`ONLY="go rust go *" ./scripts/interop-test.sh` runs a single server and
pairing.

Two narrower checks run the same way across languages:

```bash
# The Rust protocol test suite, pointed at the Go server instead of its own
(cd rust && HAMRADNET_SERVER_BIN=$PWD/../dist/go/linux/hamradnet-server \
    cargo test -p hamradnet-server --test end_to_end)

# A live encrypted handshake between Go's and Rust's Noise code
(cd rust && cargo build --release --example noise_responder -p hamradnet)
(cd go && HAMRADNET_RUST_RESPONDER=$PWD/../rust/target/release/examples/noise_responder \
    go test ./internal/session/)
```

## Layout

```
go/                    the Go version
  internal/proto       the wire protocol
  internal/server      coordinator: control plane, relay, endpoint reflection
  internal/client      daemon and local control channel, with the tun device
  internal/session     Noise IK sessions
  cmd/                 hamradnet, hamradnet-server, hamradnet-gui
rust/                  the Rust version (a Cargo workspace)
  crates/proto         the wire protocol
  crates/server        coordinator
  crates/client        daemon and CLI, with the tun device per platform
  crates/gui           the desktop app
deploy/                the server's systemd unit, shared by both versions
scripts/               build-*.sh, deploy-vps.sh, install-server.sh,
                       run-gui.sh, install-service.sh, interop-test.sh
dist/go/, dist/rust/   built programs (made by the build scripts):
  linux/               every Linux program, the server too
  linux-service/       the app + install.sh (from install-service.sh)
  linux-standalone/    the app + run-gui.sh
  windows/             the Windows programs and wintun.dll
```

On Linux the app either runs the daemon inside itself, which needs root, or
drives the background service over the local control channel. It decides at
startup by asking whether a daemon answers on `ipc.json`, which the service
gives to the installing user with `up --ipc-user`.

In both versions the app and the CLI are the same daemon with different front
ends. The GUI holds a `Daemon` handle directly and calls the same command
dispatcher the CLI reaches over its loopback socket, so there is one
implementation of "create a network", not two.

Anything that crosses a wire, whether the network protocol, `networks.json`,
the client's `config.json` or the CLI's local channel, has to stay identical in
the two versions. `go/internal/proto` and `rust/crates/proto` define the
network protocol; change one and you must change the other, then re-run
`interop-test.sh`.

## Limits

* IPv4 inside the tunnel. IPv6 packets are dropped.
* Layer 3, so protocols that need real Ethernet — IPX, NetBIOS over NetBEUI —
  will not work. Anything over IP, which is essentially everything since the
  2000s, will.
* One network's broadcast traffic goes to every member, so a network is capped at
  256 players.
