# HamRadNet guide

This guide walks you through HamRadNet from nothing to playing: renting and
setting up a server, starting the app, and getting games to find each other.
The [README](README.md) is the reference; this is the walkthrough.

- [The pieces](#the-pieces)
- [1. Set up a server from nothing](#1-set-up-a-server-from-nothing)
- [2. Start the app](#2-start-the-app)
- [3. Play together](#3-play-together)
- [Games](#games)
- [When something does not work](#when-something-does-not-work)
- [Where things are](#where-things-are)
- [Staying safe](#staying-safe)

## The pieces

HamRadNet has three programs:

| Program | Runs on | What it does |
|---|---|---|
| `hamradnet-server` | one rented server (VPS) | Knows who is in which network. Passes traffic along for players who cannot reach each other directly. |
| `hamradnet-gui` (the app) | every player's PC | Creates the `hamrad0` network adapter and connects you to your friends. |
| `hamradnet` (command line) | any PC | Does everything the app does, without a window. |

Each program exists twice: once written in **Go** and once in **Rust**. They
all work together in any mix. A Rust app connects to a Go server, and a Go
player plays with a Rust player. Pick either. If one gives you trouble on your
machine, try the other.

Your group needs **one server**. Every player needs **the app**.

## 1. Set up a server from nothing

### What to rent

The cheapest VPS you can find is enough. It needs:

- a **public IPv4 address**,
- **Debian or Ubuntu** (Fedora, Rocky, Alma and Arch work too),
- an **x86-64 or ARM** processor (either works),
- 512 MB of memory or more.

The provider gives you an **IP address** and a way to log in: usually the
`root` password, sometimes a user such as `ubuntu` plus an SSH key file.

### Install the coordinator

On your own Linux PC, once:

```bash
git clone https://github.com/finhex/hamradnet
cd hamradnet
```

You also need [Go](https://go.dev/dl/) 1.24 or newer to build the Go server, or
[Rust](https://rustup.rs) to build the Rust one.

Then run one command, with your server's IP:

```bash
./scripts/deploy-vps.sh 203.0.113.45
```

Type the server's password when asked (only once). The script then:

1. finds out whether the server is x86-64 or ARM and builds the right program,
2. uploads it,
3. creates a locked-down `hamradnet` account to run it,
4. installs it as a service that restarts by itself and starts at boot,
5. opens ports 7777 (TCP) and 7778 (UDP) in the server's firewall,
6. tests both ports from your PC.

When it ends with `Everything works.`, the server is ready.

**If your login is not root**, add the user name: `./scripts/deploy-vps.sh
203.0.113.45 ubuntu`. The script uses `sudo` on the server.

**If you log in with a key file**:
`HAMRADNET_SSH_OPTS="-i ~/.ssh/my_key" ./scripts/deploy-vps.sh 203.0.113.45 ubuntu`

**If SSH uses another port**: `./scripts/deploy-vps.sh --ssh-port 2222 203.0.113.45`

**If it says the server's identity key changed**: you probably reinstalled the
server, or the provider gave its IP to a new machine. Run the `ssh-keygen -R`
command the script prints, then run the script again. If you did not reinstall
anything, stop and find out why.

**If the final test fails but the install worked**: your provider has a
firewall in front of the server, usually called a "security group" or
"firewall" in their web panel. Allow TCP 7777 and UDP 7778 there.

### Later

| To... | Run |
|---|---|
| upgrade to a newer version | `./scripts/deploy-vps.sh 203.0.113.45` again |
| switch to the Rust server | `./scripts/deploy-vps.sh --rust 203.0.113.45` |
| switch back to Go | `./scripts/deploy-vps.sh --go 203.0.113.45` |
| see if it is running | `ssh root@203.0.113.45 systemctl status hamradnet-server` |
| watch it live | `ssh root@203.0.113.45 journalctl -u hamradnet-server -f` |
| test it from any PC | `hamradnet check --server 203.0.113.45:7777` |

Switching between Go and Rust keeps every network, ID and password.

## 2. Start the app

### Getting it

Download it from the [Releases](https://github.com/finhex/hamradnet/releases)
page, or build it yourself with `./scripts/build-all.sh`, which puts everything
in `dist/go/` and `dist/rust/`.

### Linux

Start it with the launcher next to it:

```bash
./run-gui.sh
```

From the cloned folder, that is `dist/go/linux/run-gui.sh` or
`dist/rust/linux/run-gui.sh`.

**Why it asks for your password:** the app creates a network adapter
(`hamrad0`), and Linux only lets administrators do that. The launcher asks
`sudo` for permission.

**Why it sometimes does not ask:** `sudo` remembers your password for a few
minutes (five by default) in the same terminal. If you used `sudo` recently, it
does not ask again. That is normal. To make it ask every time, run `sudo -k`
first.

**Why not just `sudo hamradnet-gui`:** `sudo` hides your screen from the app,
and on a Wayland desktop the Go app then crashes while loading your icon theme.
The launcher passes your screen through and draws the window via XWayland, so
it works and looks the same.

### Windows

1. Unzip `hamradnet-go-windows.zip` or `hamradnet-rust-windows.zip`.
2. Keep all the files in the same folder. `hamradnet-gui.exe` needs
   `wintun.dll` next to it.
3. Right-click `hamradnet-gui.exe` → **Run as administrator**.

### First start

The app asks for:

- **Server address**: just the IP, e.g. `203.0.113.45`.
- **Your name**: what your friends will see.

Both are remembered. To change server later, click **Change server** in the top
right.

## 3. Play together

1. One player clicks **Create a network** and picks a name and a password. The
   app shows an ID like `HAM-4821-9903`. **Copy ID** puts it on the clipboard.
2. Send the ID and the password to your friends privately.
3. Friends click **Join a network** and enter both.

Each network shows who is in it:

- **Address**: the address a game uses to reach that player, like `10.66.0.3`.
  Yours is the row marked *(you)*.
- **Ping**: real round-trip time to that player through the tunnel. Green is
  good (below 60 ms).
- Hover the ping to see **direct** (straight to them, fastest) or **relay**
  (through your server, adds the server's distance). HamRadNet tries for direct
  and switches by itself when it can.

**Leave** takes you out; the network stays for the others. **Delete** removes it
for everyone; only its creator can do that.

Networks are kept even when everyone goes offline, so the same ID and password
work next weekend. A network nobody has used for 14 days is removed.

## Games

Games find each other in one of two ways, and HamRadNet handles them
differently.

**1. You type the other player's address.** This always works. Use the
player's `10.66.x.x` address from the app, for example in Minecraft's Direct
Connect or a shooter's "connect to IP".

**2. The game lists LAN games by itself.** The game shouts "anyone hosting?"
to the whole network, which is called a *broadcast*. HamRadNet copies every
broadcast that enters the tunnel to all players. The question is whether your
computer sends the broadcast into the tunnel at all:

| How the game shouts | Works? |
|---|---|
| to the tunnel's own network (`10.66.255.255`) | yes |
| multicast, used by some newer games | usually; it depends on the system |
| to `255.255.255.255`: **Warcraft III** and many games from around 2000-2010 | **after one setting** (below) |

The last row needs a setting because a broadcast to `255.255.255.255` leaves
your PC through **one** network adapter only, normally your real one, so it
never enters the tunnel. Hamachi and Radmin VPN users hit the same problem.
Here is how to fix it on each system.

### Warcraft III

Everyone must run **the same game version**.

**Linux players** (including Warcraft III under Wine or Proton): after
HamRadNet has started, run

```bash
sudo ./scripts/lan-broadcast.sh up      # sends WC3's broadcasts (UDP 6112) into the tunnel
sudo ./scripts/lan-broadcast.sh down    # when you are done, back to normal
```

Only Warcraft III's port goes into the tunnel; everything else on your PC
keeps broadcasting to your home network as usual. The setting is forgotten at
reboot.

**Windows players**, in an **Administrator PowerShell** while HamRadNet runs:

```powershell
# Windows sends 255.255.255.255 broadcasts out of the adapter with the lowest
# metric. Making it HamRadNet's lets the game list find your friends.
Set-NetIPInterface -InterfaceAlias hamrad0 -InterfaceMetric 1

# To undo it:
Set-NetIPInterface -InterfaceAlias hamrad0 -AutomaticMetric Enabled
```

This is the standard fix for Hamachi and Radmin VPN. The Linux side above was
tested with HamRadNet; this Windows command has not been tested with
HamRadNet's adapter yet.

Windows also usually files the HamRadNet adapter under **public** networks.
When Windows Firewall asks about Warcraft III, tick **both Private and
Public**. Otherwise other players can see your game but cannot join it.

### Any other game whose LAN list stays empty

1. Make sure you can reach the other player: `ping 10.66.x.x` (their address).
2. Find which UDP port the game uses for LAN discovery. Searching "*game name*
   LAN port" usually finds it.
3. On Linux, give the script that port; you can list several, and ranges work:

   ```bash
   sudo ./scripts/lan-broadcast.sh up 6112 27015-27020
   ```

   On Windows, the metric setting above already covers every port.
4. If the game also has a "join by IP" option, that always works.

### What cannot work

- **IPX-only games.** Some games from the 1990s offer only IPX networking.
  HamRadNet carries IP traffic only. Most of those games also have a
  TCP/IP or UDP mode: use that.
- **IPv6-only LAN modes.** HamRadNet's network is IPv4.
- **Online-only games.** If a game needs its official servers, HamRadNet
  changes nothing about that. It is for LAN modes.

For Minecraft step by step, see the [README](README.md#playing-minecraft).

## When something does not work

Go down this list in order:

1. **The app stays on "Connecting…".** Check the server address (**Change
   server**), then test it from any PC:
   `hamradnet check --server 203.0.113.45:7777`. That test tells you whether TCP
   or UDP is blocked.
2. **A friend stays "connecting".** Wait a minute. HamRadNet keeps trying, and
   falls back to the relay by itself.
3. **Test the tunnel:** `ping 10.66.x.x` (your friend's address). If that fails,
   it is a HamRadNet problem; restart the app on both sides.
4. **Ping works, but the game list is empty.** See [Games](#games).
5. **The game is listed, but joining fails.** Check the host's firewall. On
   Windows, allow the game for both Private and Public networks.
6. **The app window does not open on Linux.** Start it with `run-gui.sh`, not
   `sudo hamradnet-gui`.

## Where things are

| What | Linux | Windows |
|---|---|---|
| Your identity, server address, saved networks and passwords | `/etc/hamradnet/config.json` (root only) | `C:\ProgramData\HamRadNet\config.json` |
| How the command line finds the running app | `/etc/hamradnet/ipc.json` | `C:\ProgramData\HamRadNet\ipc.json` |
| Network adapter | `hamrad0` | `hamrad0` |

The Go and Rust apps share these files, so you can switch apps and keep your
networks.

On the server:

| What | Where |
|---|---|
| The program | `/usr/local/bin/hamradnet-server` |
| Networks, IDs, password hashes | `/var/lib/hamradnet/networks.json` |
| The service | `systemctl status hamradnet-server` |
| Its log | `journalctl -u hamradnet-server -f` |

## Staying safe

- **The network password is the only key to your network.** Anyone with the ID
  and password can reach your PC on its `10.66.x.x` address, just like someone
  plugged into your home router. Pick a real password and share it privately.
- **Traffic between players is encrypted.** The server passes encrypted packets
  along and cannot read them.
- **Protect the server's login.** Prefer an SSH key to a password, and never
  keep the server's root password in a plain text file, least of all inside a
  folder you might share or upload.
