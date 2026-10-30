package client

import (
	"encoding/json"
	"errors"

	"github.com/finhex/hamradnet/internal/proto"
)

// The local control channel speaks exactly the Rust client's format (see
// rust/crates/client/src/ipc.rs). Both versions use the same ipc.json, so this
// is what lets either command line drive either running daemon or app.
//
// The Go types stay as they are for in-process callers like the GUI; only their
// JSON form is the Rust one:
//
//	request   {"token": "...", "command": {"cmd": "join_network", "network_id": "...", "password": "..."}}
//	response  {"reply": "status" | "peers" | "ok" | "error", ...}
//
// Rust models absent values as null (Option), where Go uses the zero value, so
// the conversion is done here in both directions. A Rust Vec must never receive
// null, so lists are always written as [].

// Command names on the wire, by the Go name.
var wireCommands = map[string]string{
	"status":    "status",
	"peers":     "peers",
	"create":    "create_network",
	"join":      "join_network",
	"leave":     "leave_network",
	"delete":    "delete_network",
	"configure": "configure",
	"logs":      "logs",
}

type wireCommand struct {
	Cmd       string `json:"cmd"`
	Name      string `json:"name,omitempty"`
	NetworkID string `json:"network_id,omitempty"`
	Password  string `json:"password,omitempty"`
	Server    string `json:"server,omitempty"`
}

func (r Request) MarshalJSON() ([]byte, error) {
	cmd, ok := wireCommands[r.Command]
	if !ok {
		cmd = r.Command
	}
	return json.Marshal(struct {
		Token   string      `json:"token"`
		Command wireCommand `json:"command"`
	}{r.Token, wireCommand{Cmd: cmd, Name: r.Name, NetworkID: r.NetworkID, Password: r.Password,
		Server: r.Server}})
}

func (r *Request) UnmarshalJSON(data []byte) error {
	var wire struct {
		Token   string      `json:"token"`
		Command wireCommand `json:"command"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*r = Request{Token: wire.Token, Command: wire.Command.Cmd,
		Name: wire.Command.Name, NetworkID: wire.Command.NetworkID, Password: wire.Command.Password,
		Server: wire.Command.Server}
	for goName, wireName := range wireCommands {
		if wireName == wire.Command.Cmd {
			r.Command = goName
		}
	}
	return nil
}

func (r Response) MarshalJSON() ([]byte, error) {
	switch {
	case !r.OK:
		return json.Marshal(map[string]any{"reply": "error", "message": r.Message})
	case r.Status != nil:
		return json.Marshal(map[string]any{"reply": "status", "status": r.Status})
	case r.Peers != nil:
		return json.Marshal(map[string]any{"reply": "peers", "peers": r.Peers})
	default:
		return json.Marshal(map[string]any{"reply": "ok", "message": r.Message})
	}
}

func (r *Response) UnmarshalJSON(data []byte) error {
	var wire struct {
		Reply   string       `json:"reply"`
		Message string       `json:"message"`
		Status  *Status      `json:"status"`
		Peers   []PeerReport `json:"peers"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.Reply == "" {
		return errors.New("the running daemon answered in a format this build does not " +
			"understand; restart it with the same version as this command")
	}
	*r = Response{OK: wire.Reply != "error", Message: wire.Message, Status: wire.Status, Peers: wire.Peers}
	if wire.Reply == "peers" && r.Peers == nil {
		r.Peers = []PeerReport{}
	}
	return nil
}

// wireStatus is Status as Rust sees it: optional values are null when absent.
type wireStatus struct {
	Connected      bool            `json:"connected"`
	Server         string          `json:"server"`
	Name           string          `json:"name"`
	PeerID         *uint32         `json:"peer_id"`
	VirtualIP      *string         `json:"virtual_ip"`
	PublicEndpoint *string         `json:"public_endpoint"`
	Networks       []NetworkReport `json:"networks"`
	PeerCount      int             `json:"peer_count"`
	DirectCount    int             `json:"direct_count"`
	ServerRTTMs    *float64        `json:"server_rtt_ms"`
	TotalRX        uint64          `json:"total_rx"`
	TotalTX        uint64          `json:"total_tx"`
	LastError      *string         `json:"last_error"`
}

func (s Status) MarshalJSON() ([]byte, error) {
	wire := wireStatus{
		Connected: s.Connected, Server: s.Server, Name: s.Name,
		PublicEndpoint: optional(s.PublicAddr), Networks: s.Networks,
		PeerCount: s.PeerCount, DirectCount: s.DirectCount,
		TotalRX: s.TotalRX, TotalTX: s.TotalTX, LastError: optional(s.LastError),
	}
	// An address is assigned together with the peer ID, so one stands for both.
	if s.VirtualIP != "" {
		id := uint32(s.PeerID)
		wire.PeerID, wire.VirtualIP = &id, &s.VirtualIP
	}
	if s.ServerRTTMs > 0 {
		wire.ServerRTTMs = &s.ServerRTTMs
	}
	if wire.Networks == nil {
		wire.Networks = []NetworkReport{}
	}
	return json.Marshal(wire)
}

func (s *Status) UnmarshalJSON(data []byte) error {
	var wire wireStatus
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*s = Status{
		Connected: wire.Connected, Server: wire.Server, Name: wire.Name,
		PublicAddr: deref(wire.PublicEndpoint), Networks: wire.Networks,
		PeerCount: wire.PeerCount, DirectCount: wire.DirectCount,
		TotalRX: wire.TotalRX, TotalTX: wire.TotalTX, LastError: deref(wire.LastError),
		VirtualIP: deref(wire.VirtualIP),
	}
	if wire.PeerID != nil {
		s.PeerID = proto.PeerID(*wire.PeerID)
	}
	if wire.ServerRTTMs != nil {
		s.ServerRTTMs = *wire.ServerRTTMs
	}
	return nil
}

func (p PeerReport) MarshalJSON() ([]byte, error) {
	// The alias has PeerReport's fields and tags but not this method, so
	// marshalling it does not recurse.
	type plain PeerReport
	wire := struct {
		plain
		RTTMs    *float64 `json:"rtt_ms"`
		Networks []string `json:"networks"`
	}{plain: plain(p), Networks: p.Networks}
	if p.RTTMs > 0 {
		wire.RTTMs = &p.RTTMs
	}
	if wire.Networks == nil {
		wire.Networks = []string{}
	}
	return json.Marshal(wire)
}

func (p *PeerReport) UnmarshalJSON(data []byte) error {
	type plain PeerReport
	wire := struct {
		*plain
		RTTMs *float64 `json:"rtt_ms"`
	}{plain: (*plain)(p)}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	p.RTTMs = 0
	if wire.RTTMs != nil {
		p.RTTMs = *wire.RTTMs
	}
	return nil
}

func optional(s string) *string {
	if s == "" {
		return nil
	}
	return &s
}

func deref(s *string) string {
	if s == nil {
		return ""
	}
	return *s
}
