// Command hamradnet-server is the HamRadNet coordinator.
//
// Run it on any machine with a public IP. It does three things:
//   - keeps the roster of networks and who is in them (TCP),
//   - tells each client what its public address looks like from outside (UDP),
//   - relays traffic for peer pairs that cannot reach each other directly (UDP).
package main

import (
	"flag"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/signal"
	"strconv"
	"syscall"
	"time"

	"github.com/finhex/hamradnet/internal/proto"
	"github.com/finhex/hamradnet/internal/server"
)

// version is stamped at build time with -ldflags "-X main.version=..."; the
// default keeps `--version` honest in a plain `go build`.
var version = "0.1.0"

// graceFlag takes plain seconds, as the Rust server's --empty-grace does, so one
// command line or systemd unit works for either server. A Go duration such as
// "336h" is accepted too.
type graceFlag time.Duration

func (g *graceFlag) String() string { return time.Duration(*g).String() }

func (g *graceFlag) Set(text string) error {
	if seconds, err := strconv.ParseUint(text, 10, 64); err == nil {
		*g = graceFlag(time.Duration(seconds) * time.Second)
		return nil
	}
	d, err := time.ParseDuration(text)
	if err != nil {
		return fmt.Errorf("want seconds, or a duration such as 336h: %w", err)
	}
	*g = graceFlag(d)
	return nil
}

func main() {
	emptyGrace := graceFlag(server.AbandonedAfter)
	var (
		bind        = flag.String("bind", "0.0.0.0", "address to bind both listeners to")
		controlPort = flag.Int("control-port", proto.DefaultControlPort, "TCP port for the control plane, 0 to let the OS choose")
		udpPort     = flag.Int("udp-port", proto.DefaultUDPPort, "UDP port for relay and reflection, 0 to let the OS choose")
		store       = flag.String("store", "networks.json", "file networks are saved to")
		noStore     = flag.Bool("no-store", false, "keep networks in memory only")
		showVersion = flag.Bool("version", false, "print the version and exit")
	)
	flag.Var(&emptyGrace, "empty-grace",
		"seconds to keep a network after its last member leaves; 0 removes it at once")
	flag.Parse()

	if *showVersion {
		fmt.Printf("hamradnet-server %s\n", version)
		return
	}

	log := slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: logLevel()}))

	storePath := *store
	if *noStore {
		storePath = ""
	}
	state := server.NewState(storePath, time.Duration(emptyGrace), log)

	listener, err := net.Listen("tcp", fmt.Sprintf("%s:%d", *bind, *controlPort))
	if err != nil {
		log.Error("binding the control plane", "err", err)
		os.Exit(1)
	}
	udpAddr, err := net.ResolveUDPAddr("udp", fmt.Sprintf("%s:%d", *bind, *udpPort))
	if err != nil {
		log.Error("resolving the udp address", "err", err)
		os.Exit(1)
	}
	udpConn, err := net.ListenUDP("udp", udpAddr)
	if err != nil {
		log.Error("binding the udp plane", "err", err)
		os.Exit(1)
	}

	// Report the addresses actually bound, which differ from those requested
	// when a port of 0 asked the OS to choose. Printed on stdout in a fixed
	// format so scripts and tests can read it without parsing the log.
	controlAddr := listener.Addr().String()
	boundUDP := udpConn.LocalAddr().(*net.UDPAddr)
	fmt.Printf("listening control=%s udp=%s\n", controlAddr, boundUDP.String())
	_ = os.Stdout.Sync()

	log.Info("hamradnet coordinator listening", "control", controlAddr, "udp", boundUDP.String())

	// Remove networks once everybody has left them.
	go func() {
		ticker := time.NewTicker(10 * time.Second)
		defer ticker.Stop()
		for range ticker.C {
			if removed := state.SweepEmpty(); removed > 0 {
				log.Info("removed empty networks", "removed", removed)
			}
		}
	}()

	// The port clients are told to use must be the one actually bound: with
	// --udp-port 0 the requested value is 0, and clients would be sent to port 0.
	go func() {
		if err := server.ServeControl(listener, state, uint16(boundUDP.Port), log); err != nil {
			log.Error("control plane stopped", "err", err)
		}
	}()
	go func() {
		if err := server.ServeRelay(udpConn, state, log); err != nil {
			log.Error("relay stopped", "err", err)
		}
	}()

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, os.Interrupt, syscall.SIGTERM)
	<-stop
	log.Info("shutting down")
	_ = listener.Close()
	_ = udpConn.Close()
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
