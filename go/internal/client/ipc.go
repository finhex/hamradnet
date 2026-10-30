package client

import (
	"crypto/rand"
	"crypto/subtle"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net"
	"net/netip"
	"os"
	"path/filepath"
	"time"

	"github.com/finhex/hamradnet/internal/proto"
)

// The CLI talks to the running daemon over loopback TCP rather than a unix
// socket or a named pipe, so one implementation covers both platforms. The
// daemon writes the chosen port and a random token to an owner-only file; a
// caller that cannot read that file cannot drive the daemon.

// Endpoint is what the daemon advertises for the CLI to find.
type Endpoint struct {
	Port  uint16 `json:"port"`
	Token string `json:"token"`
}

// Request is one command from the CLI.
type Request struct {
	Token     string `json:"token"`
	Command   string `json:"command"`
	NetworkID string `json:"network_id,omitempty"`
	Name      string `json:"name,omitempty"`
	Password  string `json:"password,omitempty"`
}

// Response is the daemon's reply.
type Response struct {
	OK      bool         `json:"ok"`
	Message string       `json:"message,omitempty"`
	Status  *Status      `json:"status,omitempty"`
	Peers   []PeerReport `json:"peers,omitempty"`
}

// Status is a snapshot of the connection.
type Status struct {
	Connected   bool            `json:"connected"`
	Server      string          `json:"server"`
	Name        string          `json:"name"`
	PeerID      proto.PeerID    `json:"peer_id"`
	VirtualIP   string          `json:"virtual_ip"`
	PublicAddr  string          `json:"public_addr"`
	Networks    []NetworkReport `json:"networks"`
	PeerCount   int             `json:"peer_count"`
	DirectCount int             `json:"direct_count"`
	ServerRTTMs float64         `json:"server_rtt_ms"`
	TotalRX     uint64          `json:"total_rx"`
	TotalTX     uint64          `json:"total_tx"`
	LastError   string          `json:"last_error,omitempty"`
}

// NetworkReport is one network in the status.
type NetworkReport struct {
	NetworkID string `json:"network_id"`
	Name      string `json:"name"`
}

// PeerReport is one player.
type PeerReport struct {
	PeerID    proto.PeerID `json:"peer_id"`
	Name      string       `json:"name"`
	VirtualIP string       `json:"virtual_ip"`
	Link      string       `json:"link"`
	Path      string       `json:"path"`
	RTTMs     float64      `json:"rtt_ms"`
	RXBytes   uint64       `json:"rx_bytes"`
	TXBytes   uint64       `json:"tx_bytes"`
	Networks  []string     `json:"networks"`
}

// Status returns a snapshot for the CLI and the GUI.
func (d *Daemon) Status() Status {
	d.mu.Lock()
	defer d.mu.Unlock()

	s := Status{
		Connected: d.connected,
		Server:    d.config.Server,
		Name:      d.config.Name,
		PeerCount: d.peers.Len(),
		LastError: d.lastError,
	}
	if d.identity != nil {
		s.PeerID = d.identity.PeerID
		s.VirtualIP = d.identity.VirtualIP.String()
	}
	if d.hasPublic {
		s.PublicAddr = d.publicAddr.String()
	}
	if d.serverRTT > 0 {
		s.ServerRTTMs = float64(d.serverRTT.Microseconds()) / 1000
	}
	for _, n := range d.networks {
		s.Networks = append(s.Networks, NetworkReport{NetworkID: n.NetworkID, Name: n.Name})
	}
	for _, id := range d.peers.IDs() {
		peer, ok := d.peers.Get(id)
		if !ok {
			continue
		}
		if peer.IsDirect() {
			s.DirectCount++
		}
		s.TotalRX += peer.RXBytes
		s.TotalTX += peer.TXBytes
	}
	return s
}

// PeerReports lists every player, for the CLI and the GUI.
func (d *Daemon) PeerReports() []PeerReport {
	d.mu.Lock()
	defer d.mu.Unlock()

	reports := make([]PeerReport, 0, d.peers.Len())
	for _, id := range d.peers.IDs() {
		peer, ok := d.peers.Get(id)
		if !ok {
			continue
		}
		networks := make([]string, 0, len(peer.Networks))
		for network := range peer.Networks {
			networks = append(networks, network)
		}
		report := PeerReport{
			PeerID:    id,
			Name:      peer.Info.Name,
			VirtualIP: peer.Info.VirtualIP.String(),
			Link:      peer.Link.String(),
			Path:      peer.PathName(),
			RXBytes:   peer.RXBytes,
			TXBytes:   peer.TXBytes,
			Networks:  networks,
		}
		if peer.RTT > 0 {
			report.RTTMs = float64(peer.RTT.Microseconds()) / 1000
		}
		reports = append(reports, report)
	}
	return reports
}

// commandTimeout is how long a create or join waits for the coordinator.
const commandTimeout = 10 * time.Second

// Dispatch runs one command. Shared by the local socket and any in-process UI.
func (d *Daemon) Dispatch(req Request) Response {
	switch req.Command {
	case "status":
		s := d.Status()
		return Response{OK: true, Status: &s}

	case "peers":
		return Response{OK: true, Peers: d.PeerReports()}

	case "create":
		if !d.isConnected() {
			return notConnected()
		}
		if req.Password == "" {
			return Response{Message: "a password is required"}
		}
		events := d.subscribe()
		defer d.unsubscribe(events)
		d.sendToServer(proto.MsgCreateNetwork, map[string]any{
			"name": req.Name, "password": req.Password,
		})
		created, ok := waitFor(events, "created")
		if !ok {
			return Response{Message: created.Message}
		}
		// Creating a network does not put us in it, so join as well: that is
		// what saves the password and brings the tunnel up.
		d.mu.Lock()
		d.pendingJoins[created.NetworkID] = req.Password
		d.mu.Unlock()
		d.sendToServer(proto.MsgJoinNetwork, map[string]any{
			"network_id": created.NetworkID, "password": req.Password,
		})
		return Response{OK: true, Message: fmt.Sprintf(
			"Created network %q.\n\n  Network ID: %s\n\nShare that ID and the password with your friends.",
			created.Name, created.NetworkID)}

	case "join":
		if !d.isConnected() {
			return notConnected()
		}
		events := d.subscribe()
		defer d.unsubscribe(events)
		d.mu.Lock()
		d.pendingJoins[req.NetworkID] = req.Password
		d.mu.Unlock()
		d.sendToServer(proto.MsgJoinNetwork, map[string]any{
			"network_id": req.NetworkID, "password": req.Password,
		})
		joined, ok := waitFor(events, "joined")
		if !ok {
			return Response{Message: joined.Message}
		}
		return Response{OK: true, Message: fmt.Sprintf(
			"Joined %q (%s).\nYour address on this network is %s.",
			joined.Name, joined.NetworkID, d.Status().VirtualIP)}

	case "leave":
		if !d.isConnected() {
			return notConnected()
		}
		d.sendToServer(proto.MsgLeaveNetwork, map[string]any{"network_id": req.NetworkID})
		return Response{OK: true, Message: "Left " + req.NetworkID + "."}

	case "delete":
		if !d.isConnected() {
			return notConnected()
		}
		events := d.subscribe()
		defer d.unsubscribe(events)
		d.sendToServer(proto.MsgDeleteNetwork, map[string]any{"network_id": req.NetworkID})
		deleted, ok := waitFor(events, "deleted")
		if !ok {
			return Response{Message: deleted.Message}
		}
		return Response{OK: true, Message: "Deleted " + deleted.NetworkID + ". Nobody can join it again."}

	default:
		return Response{Message: "unknown command " + req.Command}
	}
}

func notConnected() Response {
	return Response{Message: "not connected to the coordinator yet; check `hamradnet status`"}
}

func (d *Daemon) isConnected() bool {
	d.mu.Lock()
	defer d.mu.Unlock()
	return d.connected
}

// waitFor blocks until the wanted outcome arrives, a failure arrives, or the
// coordinator stays silent for too long.
func waitFor(events chan Event, want string) (Event, bool) {
	deadline := time.After(commandTimeout)
	for {
		select {
		case e := <-events:
			switch e.Kind {
			case want:
				return e, true
			case "failed":
				return e, false
			}
		case <-deadline:
			return Event{Message: "the coordinator did not answer in time"}, false
		}
	}
}

func (d *Daemon) subscribe() chan Event {
	ch := make(chan Event, 16)
	d.mu.Lock()
	d.subscribers = append(d.subscribers, ch)
	d.mu.Unlock()
	return ch
}

func (d *Daemon) unsubscribe(ch chan Event) {
	d.mu.Lock()
	kept := d.subscribers[:0]
	for _, existing := range d.subscribers {
		if existing != ch {
			kept = append(kept, existing)
		}
	}
	d.subscribers = kept
	d.mu.Unlock()
}

// ServeIPC runs the local control socket until the listener closes.
func (d *Daemon) ServeIPC() error {
	// Port 0: the OS picks a free port, which is published in the endpoint file.
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return err
	}
	defer ln.Close()

	var raw [32]byte
	if _, err := rand.Read(raw[:]); err != nil {
		return err
	}
	token := base64.StdEncoding.EncodeToString(raw[:])
	port := uint16(ln.Addr().(*net.TCPAddr).Port)

	path := IPCPath()
	if err := SaveEndpoint(path, Endpoint{Port: port, Token: token}); err != nil {
		return err
	}
	d.log.Info("local control channel ready", "port", port, "path", path)

	for {
		conn, err := ln.Accept()
		if err != nil {
			return err
		}
		go d.handleIPC(conn, token)
	}
}

func (d *Daemon) handleIPC(conn net.Conn, token string) {
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(commandTimeout + 5*time.Second))

	frame, err := proto.ReadFrame(conn)
	if err != nil {
		return
	}
	var req Request
	if err := json.Unmarshal(frame, &req); err != nil {
		return
	}
	// Anyone who can read the endpoint file may drive the daemon; anyone who
	// cannot, may not.
	if subtle.ConstantTimeCompare([]byte(req.Token), []byte(token)) != 1 {
		reply, _ := json.Marshal(Response{Message: "bad token"})
		_ = proto.WriteFrame(conn, reply)
		return
	}

	reply, err := json.Marshal(d.Dispatch(req))
	if err != nil {
		return
	}
	_ = proto.WriteFrame(conn, reply)
}

// SaveEndpoint writes the endpoint file, owner-only: it holds the token.
func SaveEndpoint(path string, e Endpoint) error {
	if dir := filepath.Dir(path); dir != "" {
		if err := os.MkdirAll(dir, 0o700); err != nil {
			return err
		}
	}
	raw, err := json.MarshalIndent(e, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, raw, 0o600)
}

// LoadEndpoint reads where the daemon is listening.
func LoadEndpoint(path string) (Endpoint, error) {
	var e Endpoint
	raw, err := os.ReadFile(path)
	if err != nil {
		return e, fmt.Errorf("cannot read %s. Is the daemon running? Start it with `hamradnet up`", path)
	}
	return e, json.Unmarshal(raw, &e)
}

// SendCommand sends one command to a running daemon and returns its reply.
func SendCommand(path string, req Request) (Response, error) {
	var resp Response
	endpoint, err := LoadEndpoint(path)
	if err != nil {
		return resp, err
	}
	conn, err := net.DialTimeout("tcp",
		netip.AddrPortFrom(netip.AddrFrom4([4]byte{127, 0, 0, 1}), endpoint.Port).String(),
		5*time.Second)
	if err != nil {
		return resp, fmt.Errorf("connecting to the daemon: %w", err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(commandTimeout + 5*time.Second))

	req.Token = endpoint.Token
	raw, err := json.Marshal(req)
	if err != nil {
		return resp, err
	}
	if err := proto.WriteFrame(conn, raw); err != nil {
		return resp, err
	}
	frame, err := proto.ReadFrame(conn)
	if err != nil {
		return resp, err
	}
	return resp, json.Unmarshal(frame, &resp)
}
