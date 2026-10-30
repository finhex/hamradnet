// Command hamradnet is the HamRadNet client.
//
// `hamradnet up` runs the daemon, which owns the tunnel. Every other subcommand
// is a thin client that talks to that daemon over a loopback socket.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/signal"
	"os/user"
	"sort"
	"strconv"
	"strings"
	"syscall"

	"github.com/finhex/hamradnet/internal/client"
)

var version = "0.4.0-go"

func main() {
	if len(os.Args) < 2 {
		usage()
		os.Exit(2)
	}

	command := os.Args[1]
	args := os.Args[2:]

	var err error
	switch command {
	case "up":
		err = runDaemon(args)
	case "create":
		err = runCreate(args)
	case "join":
		err = runJoin(args)
	case "leave":
		err = runSimple(args, "leave", "leave <network-id>")
	case "delete":
		err = runSimple(args, "delete", "delete <network-id>")
	case "status":
		err = runStatus()
	case "peers":
		err = runPeers()
	case "check":
		err = runCheck(args)
	case "connect":
		err = runConnect(args)
	case "logs":
		err = call(client.Request{Command: "logs"})
	case "key":
		err = runKey()
	case "version", "--version", "-v":
		fmt.Printf("hamradnet %s\n", version)
	case "help", "--help", "-h":
		usage()
	default:
		fmt.Fprintf(os.Stderr, "unknown command %q\n\n", command)
		usage()
		os.Exit(2)
	}

	if err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Fprint(os.Stderr, `hamradnet - a virtual LAN for playing games with friends

  up --server HOST:PORT --name NAME   run the daemon (needs root/Administrator)
  connect --server HOST [--name NAME] point the running daemon at a server
  create NAME --password PW           make a network and join it
  join ID --password PW               join somebody's network
  leave ID                            leave, keeping the network for others
  delete ID                           remove a network for everyone (creator only)
  status                              connection state and your address
  peers                               everyone reachable, and how
  check --server HOST:PORT            test a server's TCP and UDP (no root needed)
  logs                                the running daemon's recent log
  key                                 this machine's public key

The daemon remembers the networks you joined and rejoins them at startup, so
`+"`up`"+` is normally all you need after the first time. Run as a service, it
may start with no server at all; `+"`connect`"+` or the app then chooses one.
`)
}

func runDaemon(args []string) error {
	fs := flag.NewFlagSet("up", flag.ExitOnError)
	server := fs.String("server", "", "coordinator address, host:port (saved on first run)")
	name := fs.String("name", "", "name your friends see (saved on first run)")
	port := fs.Int("port", 0, "local UDP port, 0 lets the OS choose")
	configPath := fs.String("config", client.DefaultConfigPath(), "path to the config file")
	iface := fs.String("interface", client.InterfaceName, "name of the tunnel interface to create")
	ipcUser := fs.String("ipc-user", "", "let this user's app and command line control the daemon "+
		"(for running it as a service)")
	_ = fs.Parse(args)
	*server = client.NormalizeServer(*server)

	// The log also goes into a buffer the app can fetch, since a service's
	// output otherwise only reaches the system journal.
	logs := &client.LogBuffer{}
	log := slog.New(slog.NewTextHandler(io.MultiWriter(os.Stderr, logs),
		&slog.HandlerOptions{Level: logLevel()}))

	config, err := client.LoadConfig(*configPath)
	if err != nil {
		// First run: an identity key is generated and kept from here on. With no
		// server yet, as when started as a service, the daemon waits for one.
		// A service runs as root, so the user it serves names it, not "root".
		first := *name
		if first == "" {
			first = *ipcUser
		}
		config, err = client.GenerateConfig(*server, defaultName(first))
		if err != nil {
			return err
		}
		if err := config.Save(*configPath); err != nil {
			return err
		}
		log.Info("created a new configuration", "path", *configPath)
	} else {
		// Flags override what is on disk, and the change sticks.
		changed := false
		if *server != "" && config.Server != *server {
			config.Server, changed = *server, true
		}
		if *name != "" && config.Name != *name {
			config.Name, changed = *name, true
		}
		if changed {
			if err := config.Save(*configPath); err != nil {
				return err
			}
		}
	}

	public, err := config.PublicKey()
	if err != nil {
		return err
	}
	log.Info("starting hamradnet", "server", config.Server, "name", config.Name,
		"key", client.EncodeKey(public))

	daemon := client.NewDaemon(config, *configPath, log)
	daemon.SetInterfaceName(*iface)
	daemon.SetLogBuffer(logs)
	if *ipcUser != "" {
		owner, err := user.Lookup(*ipcUser)
		if err != nil {
			return fmt.Errorf("--ipc-user: %w", err)
		}
		uid, _ := strconv.Atoi(owner.Uid)
		gid, _ := strconv.Atoi(owner.Gid)
		daemon.SetIPCOwner(uid, gid)
	}
	defer daemon.Close()

	go func() {
		if err := daemon.ServeIPC(); err != nil {
			log.Error("local control channel stopped", "err", err)
		}
	}()

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	return daemon.Run(ctx, *port)
}

// runConnect points a running daemon at a server, the command-line twin of the
// app's Connect button.
func runConnect(args []string) error {
	fs := flag.NewFlagSet("connect", flag.ExitOnError)
	server := fs.String("server", "", "the server's address; port 7777 is assumed")
	name := fs.String("name", "", "the name your friends see (unchanged if not given)")
	_ = fs.Parse(args)
	if *server == "" {
		return errors.New("usage: connect --server HOST[:PORT] [--name NAME]")
	}
	return call(client.Request{Command: "configure", Server: *server, Name: *name})
}

func runCreate(args []string) error {
	if len(args) < 1 {
		return errors.New("usage: create <name> --password <pw>")
	}
	name := args[0]
	fs := flag.NewFlagSet("create", flag.ExitOnError)
	password := fs.String("password", "", "password friends will need in order to join")
	_ = fs.Parse(args[1:])
	if *password == "" {
		return errors.New("a --password is required")
	}
	return call(client.Request{Command: "create", Name: name, Password: *password})
}

func runJoin(args []string) error {
	if len(args) < 1 {
		return errors.New("usage: join <network-id> --password <pw>")
	}
	id := strings.ToUpper(args[0])
	fs := flag.NewFlagSet("join", flag.ExitOnError)
	password := fs.String("password", "", "the network's password")
	_ = fs.Parse(args[1:])
	return call(client.Request{Command: "join", NetworkID: id, Password: *password})
}

func runSimple(args []string, command, usageLine string) error {
	if len(args) < 1 {
		return errors.New("usage: " + usageLine)
	}
	return call(client.Request{Command: command, NetworkID: strings.ToUpper(args[0])})
}

func runKey() error {
	config, err := client.LoadConfig(client.DefaultConfigPath())
	if err != nil {
		return err
	}
	public, err := config.PublicKey()
	if err != nil {
		return err
	}
	fmt.Println(client.EncodeKey(public))
	return nil
}

func call(req client.Request) error {
	resp, err := client.SendCommand(client.IPCPath(), req)
	if err != nil {
		return err
	}
	if !resp.OK {
		return errors.New(resp.Message)
	}
	if resp.Message != "" {
		fmt.Println(resp.Message)
	}
	return nil
}

func runStatus() error {
	resp, err := client.SendCommand(client.IPCPath(), client.Request{Command: "status"})
	if err != nil {
		return err
	}
	if resp.Status == nil {
		return errors.New(resp.Message)
	}
	s := resp.Status

	state := "disconnected"
	if s.Connected {
		state = "connected"
	}
	fmt.Printf("coordinator : %s (%s)\n", s.Server, state)
	fmt.Printf("name        : %s\n", s.Name)
	if s.VirtualIP != "" {
		fmt.Printf("address     : %s\n", s.VirtualIP)
	}
	if s.PublicAddr != "" {
		fmt.Printf("public addr : %s\n", s.PublicAddr)
	}
	if s.ServerRTTMs > 0 {
		fmt.Printf("server ping : %.0f ms\n", s.ServerRTTMs)
	}
	fmt.Printf("peers       : %d (%d direct, %d relayed)\n",
		s.PeerCount, s.DirectCount, s.PeerCount-s.DirectCount)
	if len(s.Networks) == 0 {
		fmt.Println("networks    : none - use `hamradnet join <id> --password <pw>`")
	} else {
		fmt.Println("networks    :")
		sort.Slice(s.Networks, func(i, j int) bool {
			return s.Networks[i].NetworkID < s.Networks[j].NetworkID
		})
		for _, n := range s.Networks {
			fmt.Printf("  %s  %s\n", n.NetworkID, n.Name)
		}
	}
	if s.LastError != "" {
		fmt.Printf("last error  : %s\n", s.LastError)
	}
	return nil
}

func runPeers() error {
	resp, err := client.SendCommand(client.IPCPath(), client.Request{Command: "peers"})
	if err != nil {
		return err
	}
	if len(resp.Peers) == 0 {
		fmt.Println("No peers yet. Nobody else has joined your networks.")
		return nil
	}
	sort.Slice(resp.Peers, func(i, j int) bool {
		return resp.Peers[i].VirtualIP < resp.Peers[j].VirtualIP
	})

	fmt.Printf("%-16s %-18s %-12s %-8s %8s %10s %10s\n",
		"ADDRESS", "NAME", "LINK", "PATH", "PING", "RX", "TX")
	for _, p := range resp.Peers {
		ping := "-"
		if p.RTTMs > 0 {
			ping = fmt.Sprintf("%.0f ms", p.RTTMs)
		}
		fmt.Printf("%-16s %-18s %-12s %-8s %8s %10s %10s\n",
			p.VirtualIP, truncate(p.Name, 18), p.Link, p.Path, ping,
			humanBytes(p.RXBytes), humanBytes(p.TXBytes))
	}
	return nil
}

func truncate(text string, width int) string {
	runes := []rune(text)
	if len(runes) <= width {
		return text
	}
	return string(runes[:width-1]) + "…"
}

func humanBytes(n uint64) string {
	units := []string{"B", "KB", "MB", "GB", "TB"}
	value := float64(n)
	unit := 0
	for value >= 1024 && unit < len(units)-1 {
		value /= 1024
		unit++
	}
	if unit == 0 {
		return fmt.Sprintf("%d B", n)
	}
	return fmt.Sprintf("%.1f %s", value, units[unit])
}

func defaultName(given string) string {
	if given != "" {
		return given
	}
	for _, key := range []string{"HOSTNAME", "COMPUTERNAME", "USER", "USERNAME"} {
		if v := os.Getenv(key); v != "" {
			return v
		}
	}
	if host, err := os.Hostname(); err == nil && host != "" {
		return host
	}
	return "player"
}

func logLevel() slog.Level {
	switch os.Getenv("HAMRADNET_LOG") {
	case "debug":
		return slog.LevelDebug
	case "warn":
		return slog.LevelWarn
	case "error":
		return slog.LevelError
	default:
		return slog.LevelInfo
	}
}
