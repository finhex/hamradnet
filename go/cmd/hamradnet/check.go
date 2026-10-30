package main

// `hamradnet check` verifies a coordinator is reachable and working.
//
// It runs without root and without touching the tunnel interface, so it is safe
// to run anywhere. It tests both planes, which matters: the control plane is TCP
// and the traffic that actually carries your games is UDP, and a firewall that
// allows one while dropping the other is the most common way a setup fails.
//
// The output matches the Rust client's `check`, so the two can be compared.

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"math/rand/v2"
	"net"
	"net/netip"
	"sort"
	"time"

	"github.com/finhex/hamradnet/internal/client"
	"github.com/finhex/hamradnet/internal/proto"
	"github.com/finhex/hamradnet/internal/session"
)

const (
	checkConnectTimeout = 10 * time.Second
	checkUDPTimeout     = 5 * time.Second
	// Probes sent when measuring. The first is discarded as a warm-up.
	checkProbeCount = 8
)

func runCheck(args []string) error {
	fs := flag.NewFlagSet("check", flag.ExitOnError)
	server := fs.String("server", "", "coordinator address, host:port (defaults to the one in the config)")
	_ = fs.Parse(args)

	if *server == "" {
		config, err := client.LoadConfig(client.DefaultConfigPath())
		if err != nil {
			return fmt.Errorf("no --server given and no config file to take one from: %w", err)
		}
		*server = config.Server
	}
	return check(*server)
}

func check(server string) error {
	fmt.Printf("Checking %s\n\n", server)

	// ------------------------------------------------------------ TCP ----
	started := time.Now()
	conn, err := net.DialTimeout("tcp", server, checkConnectTimeout)
	if err != nil {
		return fmt.Errorf("could not connect to %s: %w", server, err)
	}
	defer conn.Close()
	fmt.Printf("  [ok]   TCP  connected in %d ms\n", time.Since(started).Milliseconds())
	serverIP := conn.RemoteAddr().(*net.TCPAddr).IP

	// A throwaway identity: this connection joins nothing and leaves no trace
	// beyond a peer that disconnects a moment later.
	_, public, err := session.GenerateKeypair()
	if err != nil {
		return err
	}
	hello, err := proto.MarshalServerMsg(proto.MsgHello, map[string]any{
		"version":    proto.Version,
		"public_key": proto.PublicKey(public),
		"name":       "check",
	})
	if err != nil {
		return err
	}
	if err := proto.WriteFrame(conn, hello); err != nil {
		return err
	}

	_ = conn.SetReadDeadline(time.Now().Add(checkConnectTimeout))
	frame, err := proto.ReadFrame(conn)
	if err != nil {
		return fmt.Errorf("the server did not reply: %w", err)
	}
	var welcome struct {
		T         string `json:"t"`
		Version   uint32 `json:"version"`
		PeerID    uint32 `json:"peer_id"`
		VirtualIP string `json:"virtual_ip"`
		UDPPort   uint16 `json:"udp_port"`
		Message   string `json:"message"`
	}
	if err := json.Unmarshal(frame, &welcome); err != nil {
		return fmt.Errorf("unreadable reply from the server: %w", err)
	}
	switch welcome.T {
	case proto.MsgWelcome:
	case proto.MsgError:
		return fmt.Errorf("the server refused us: %s", welcome.Message)
	default:
		return fmt.Errorf("unexpected reply from the server: %q", welcome.T)
	}
	if welcome.Version != proto.Version {
		fmt.Printf("  [!!]   the server speaks protocol v%d, this build speaks v%d\n",
			welcome.Version, proto.Version)
		return errors.New("version mismatch - update whichever side is older")
	}
	fmt.Printf("  [ok]   control plane speaking protocol v%d\n", proto.Version)
	fmt.Printf("         it would give this machine %s (as peer %d)\n", welcome.VirtualIP, welcome.PeerID)

	// ------------------------------------------------------------ UDP ----
	socket, err := net.ListenUDP("udp4", &net.UDPAddr{})
	if err != nil {
		return fmt.Errorf("binding a local UDP socket: %w", err)
	}
	defer socket.Close()
	serverUDP := &net.UDPAddr{IP: serverIP, Port: int(welcome.UDPPort)}

	// Several probes, not one.
	//
	// The first packet on a fresh socket pays for the NAT mapping being created
	// and the route being warmed, which can be several times the steady-state
	// round trip. Reporting that single figure as "the latency" is simply wrong,
	// so the first sample is discarded and the rest are summarised.
	var samples []time.Duration
	buf := make([]byte, proto.MaxPacketSize)
	lost := 0

	for probe := 0; probe < checkProbeCount; probe++ {
		txid := rand.Uint64()
		sent := time.Now()
		request := proto.Packet{Type: proto.TypeReflectRequest, TxID: txid}.Encode(nil)
		if _, err := socket.WriteToUDP(request, serverUDP); err != nil {
			return fmt.Errorf("sending UDP to %s: %w", serverUDP, err)
		}

		var observed netip.AddrPort
		answered := false
		deadline := sent.Add(checkUDPTimeout)
		for !answered {
			_ = socket.SetReadDeadline(deadline)
			n, _, err := socket.ReadFromUDP(buf)
			if err != nil {
				break
			}
			packet, err := proto.Decode(buf[:n])
			// A late answer to an earlier probe is not this one's.
			if err == nil && packet.Type == proto.TypeReflectResponse && packet.TxID == txid {
				observed, answered = packet.Observed, true
			}
		}

		switch {
		case !answered:
			lost++
		case probe == 0:
			// Skip the first sample: it carries the setup cost.
			fmt.Printf("  [ok]   UDP  port %d answered\n", welcome.UDPPort)
			fmt.Printf("         this machine looks like %s from outside\n", observed)
		default:
			samples = append(samples, time.Since(sent))
		}
		time.Sleep(120 * time.Millisecond)
	}

	if len(samples) == 0 {
		fmt.Printf("  [FAIL] UDP  no reply from port %d\n\n", welcome.UDPPort)
		fmt.Println("The control plane works but UDP does not get through, so peers")
		fmt.Println("would see each other and still not exchange any traffic.")
		fmt.Printf("Open UDP %d on the server and in your provider's firewall\n", welcome.UDPPort)
		fmt.Println("or security group, then run this again.")
		return fmt.Errorf("UDP %d is not reachable", welcome.UDPPort)
	}

	sort.Slice(samples, func(i, j int) bool { return samples[i] < samples[j] })
	best, median, worst := samples[0], samples[len(samples)/2], samples[len(samples)-1]
	ms := func(d time.Duration) float64 { return float64(d) / float64(time.Millisecond) }

	fmt.Printf("  [ok]   UDP  round trip  best %.0f ms · median %.0f ms · worst %.0f ms\n",
		ms(best), ms(median), ms(worst))
	if lost > 0 {
		fmt.Printf("  [!!]   %d of %d probes went missing\n", lost, checkProbeCount)
	}

	fmt.Printf("\nEverything works. Point clients at %s\n\n", server)
	// What the number does and does not mean, because it is easy to read too
	// much into it.
	fmt.Println("That is the round trip to the server, which is what relayed peers")
	fmt.Println("pay. Players who connect directly to each other never touch it, so")
	fmt.Println("their latency is the distance between them, not to here.")
	if jitter := worst - best; jitter > 60*time.Millisecond {
		fmt.Printf("\nJitter of %.0f ms is high; the path to this server is unsteady.\n", ms(jitter))
	}
	return nil
}
