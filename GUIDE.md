# HamRadNet guide

This guide walks you through HamRadNet from nothing to playing: renting and
setting up a server, starting the app, and getting games to find each other.
The [README](README.md) is the reference; this is the walkthrough.

- [The pieces](#the-pieces)
- [What it runs on](#what-it-runs-on)
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

## What it runs on

| Program | Runs on |
|---|---|
| **The app, Linux** | any 64-bit PC distro from about 2018 on: Ubuntu 20.04 and newer, Debian 10+, Linux Mint 20+, Fedora, Arch, Manjaro, CachyOS, openSUSE, Rocky/Alma/RHEL 8+ |
| **The app, Windows** | Windows 10 and 11, 64-bit |
| **The command line** | any 64-bit Linux at all, and Windows 10 and 11 |
| **The server** | any Linux with systemd, on a 64-bit PC or ARM processor |

Both Linux apps, Go and Rust, were tested on Debian 11 from 2021 and on
CachyOS from 2026.

The Linux app needs an ordinary desktop (KDE, GNOME, Xfce, Cinnamon, ...),
which already includes everything it uses, plus `sudo` or `pkexec` to ask for
your password. On a stripped-down system, the app names any library it cannot
find. On Debian or Ubuntu, this installs them all:

```bash
sudo apt install libgl1 libegl1 libx11-6 libxcursor1 libxrandr2 libxinerama1 \
    libxi6 libxxf86vm1 libxkbcommon-x11-0
```

Not built yet: the app for macOS, and for ARM computers such as a Raspberry Pi.

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
[Rust](https://rustup.rs) 1.95 or newer to build the Rust one. See [Build
tools](README.md#build-tools) for everything the full builds use.

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
in `dist/go/` and `dist/rust/` (`linux-service/`, `linux-standalone/` and
`windows/` in each).

### Linux: two ways to run it

The app needs administrator rights to create its network adapter (`hamrad0`).
There are two downloads that handle that differently. The app inside is the
same.

| | **Service** (recommended) | **Standalone** |
|---|---|---|
| Download | `hamradnet-…-linux-service` | `hamradnet-…-linux-standalone` |
| Set up | once: `sudo ./install.sh` | nothing |
| Start | from your app menu, no password | `./run-gui.sh`, password every time |
| Connection | stays up when you close the app, and starts at boot | ends when you close the app |
| Remove | **Uninstall HamRadNet** in the app menu | delete the folder |

#### Service

```bash
sudo ./install.sh
```

Run that once, inside the unpacked folder. It installs a small background
service that keeps the tunnel up, adds **HamRadNet** to your app menu, and
lets HamRadNet traffic through your firewall (ufw or firewalld). From then on,
open HamRadNet from the menu like any app; it asks for nothing.

- **To upgrade**, or to switch between the Go and Rust versions, run the new
  download's `install.sh`. Your identity, server and networks are kept.
- **To remove everything**, including the service and the firewall rule, choose
  **Uninstall HamRadNet** in the app menu, or run `hamradnet-uninstall`. It
  keeps your identity and networks in `/etc/hamradnet` for a reinstall;
  `hamradnet-uninstall --purge` removes those too.

The service also works without the app: `hamradnet connect --server
203.0.113.45`, `hamradnet status`, `hamradnet logs`, and so on, all as your
normal user.

#### Standalone

Start it with the launcher next to it:

```bash
./run-gui.sh
```

From the cloned folder, that is `dist/go/linux/run-gui.sh` or
`dist/rust/linux/run-gui.sh`.

If your firewall drops incoming traffic, as ufw does by default, let the
tunnel in once, or you will see your friends' games but not be able to join
them (and they will not see yours):

```bash
sudo ufw allow in on hamrad0
```

**Why it asks for your password:** the app creates a network adapter
(`hamrad0`), and Linux only lets administrators do that. The launcher asks
`sudo` for permission, or, on a system without `sudo`, shows a password window
through `pkexec`.

**Why it sometimes does not ask:** `sudo` remembers your password for a few
minutes (five by default) in the same terminal. If you used `sudo` recently, it
does not ask again. That is normal. To make it ask every time, run `sudo -k`
first.

**Why not just `sudo hamradnet-gui`:** `sudo` hides your screen from the app.
The launcher passes your screen through and draws the window via X11 (XWayland
on a Wayland desktop), so it works and looks the same.

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
to the whole network, which is called a *broadcast*. HamRadNet sends every
broadcast to all players, **with no setup**:

| How the game shouts | Works? |
|---|---|
| to `255.255.255.255`: **Warcraft III** and many games from around 2000-2010 | yes, by itself (see below) |
| to the tunnel's own network (`10.66.255.255`) | yes |
| multicast, used by some newer games | usually; it depends on the system |

The first row needed special care, because your computer sends a broadcast to
`255.255.255.255` out of **one** network adapter only, normally your real one,
so it would never enter the tunnel. Hamachi and Radmin VPN users hit exactly
this. HamRadNet handles it by itself:

- **Linux:** the app notices broadcasts leaving through your other adapters,
  and also sends a copy through the tunnel, from your `10.66.x.x` address so
  that answers come back the same way. The original still reaches your home
  network, so nothing there changes. Broadcasts from services that announce
  your PC itself stay at home: DHCP, Windows file sharing, KDE Connect,
  Syncthing and Steam's streaming discovery.
- **Windows:** the app makes `hamrad0` the adapter Windows prefers for
  broadcasts while it runs. That is the fix Hamachi and Radmin VPN users apply
  by hand. Broadcasts then go to your friends instead of your home network
  until you close the app.

The Linux side is tested between every pairing of Go and Rust apps and
servers. The Windows side has not been tested on Windows yet.

### Warcraft III

1. Everyone runs **the same game version**.
2. Everyone starts HamRadNet and joins the same network.
3. One player creates a LAN game; the others see it in their LAN list.

If the list stays empty, open **Log** in the app and check for a line saying
it cannot copy LAN broadcasts. Then see [When something does not
work](#when-something-does-not-work).

Windows usually files the HamRadNet adapter under **public** networks. When
Windows Firewall asks about Warcraft III, tick **both Private and Public**.
Otherwise other players can see your game but cannot join it.

### Any other game

Most games work the same way as Warcraft III. If one does not:

1. Check that you can reach the other player: `ping 10.66.x.x` (their address).
2. If the game has a "join by IP" option, use their `10.66.x.x` address. That
   always works.
3. Check the host's firewall (see above).

### What cannot work

- **IPX-only games.** Some games from the 1990s offer only IPX networking.
  HamRadNet carries IP traffic only. Most of those games also have a
  TCP/IP or UDP mode: use that.
- **IPv6-only LAN modes.** HamRadNet's network is IPv4.
- **Online-only games.** If a game needs its official servers, HamRadNet
  changes nothing about that. It is for LAN modes.

For Minecraft step by step, see the [README](README.md#playing-minecraft).

## When something does not work

**Start with the log.** Click **Log** in the app (top right, or next to
**Connect**). It shows what the app has been doing; **Copy all** puts it on
the clipboard, ready to paste into a chat or a GitHub issue for whoever is
helping you.

Then go down this list in order:

1. **The app stays on "Connecting…".** Check the server address (**Change
   server**), then test it from any PC:
   `hamradnet check --server 203.0.113.45:7777`. That test tells you whether TCP
   or UDP is blocked.
2. **A friend stays "connecting".** Wait a minute. HamRadNet keeps trying, and
   falls back to the relay by itself.
3. **Test the tunnel:** `ping 10.66.x.x` (your friend's address). If that fails,
   it is a HamRadNet problem; restart the app on both sides.
4. **Ping works, but one of you cannot see the other's game, or joining
   fails.** It is almost always a firewall on the side that cannot be reached.
   A ping still gets through, because firewalls let pings in. On Linux with
   ufw, `sudo ufw allow in on hamrad0` (the service's `install.sh` does this
   for you). To see whether ufw is the cause, run `journalctl -k | grep 'UFW
   BLOCK.*hamrad0'`: each line is a packet from a friend that it threw away.
   On Windows, allow the game for both Private and Public networks.
5. **The game list is empty on both sides.** See [Games](#games).
6. **The app window does not open on Linux (standalone version).** Start it
   with `run-gui.sh`, not `sudo hamradnet-gui`.
7. **The app says it needs administrator rights, or that the HamRadNet service
   is not answering (service version).** The service is not running. See why
   with `systemctl status hamradnet` and `journalctl -u hamradnet -n 50`, then
   start it with `sudo systemctl start hamradnet`. If it was uninstalled, run
   `sudo ./install.sh` again.

## Where things are

| What | Linux | Windows |
|---|---|---|
| Your identity, server address, saved networks and passwords | `/etc/hamradnet/config.json` (root only) | `C:\ProgramData\HamRadNet\config.json` |
| How the command line finds the running app | `/etc/hamradnet/ipc.json` | `C:\ProgramData\HamRadNet\ipc.json` |
| Network adapter | `hamrad0` | `hamrad0` |

The Go and Rust apps share these files, so you can switch apps and keep your
networks.

With the Linux service installed:

| What | Where |
|---|---|
| Is it running? | `systemctl status hamradnet` |
| Its log | `hamradnet logs` (or **Log** in the app), or `journalctl -u hamradnet -f` |
| The programs | `/usr/local/bin/hamradnet`, `hamradnet-gui`, `hamradnet-uninstall` |
| The service file | `/etc/systemd/system/hamradnet.service` |
| The menu entries | `/usr/local/share/applications/hamradnet.desktop` and `hamradnet-uninstall.desktop` |

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
