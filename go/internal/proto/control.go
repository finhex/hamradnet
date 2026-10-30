package proto

import (
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"math/rand"
	"net/netip"
)

// MaxFrameSize caps a single control frame so a hostile peer cannot make us
// allocate without bound.
const MaxFrameSize = 4 * 1024 * 1024

// The control plane is length-prefixed JSON over TCP: [len: u32 big-endian][json].
//
// The Rust side models these as serde internally-tagged enums, which put the
// variant name in a "t" field alongside that variant's own fields, flat in the
// same object. That is what the marshalling below reproduces; the field sets per
// variant have to match exactly, because serde rejects a variant that is missing
// one of its fields.

// Client -> server variant names.
const (
	MsgHello           = "hello"
	MsgCreateNetwork   = "create_network"
	MsgJoinNetwork     = "join_network"
	MsgLeaveNetwork    = "leave_network"
	MsgDeleteNetwork   = "delete_network"
	MsgUpdateEndpoints = "update_endpoints"
	MsgPing            = "ping"
)

// Server -> client variant names.
const (
	MsgWelcome        = "welcome"
	MsgNetworkCreated = "network_created"
	MsgJoined         = "joined"
	MsgLeft           = "left"
	MsgDeleted        = "deleted"
	MsgPeerUpdate     = "peer_update"
	MsgPeerLeft       = "peer_left"
	MsgError          = "error"
	MsgPong           = "pong"
)

// Error codes, matching the Rust ErrorCode enum.
const (
	ErrVersionMismatch = "version_mismatch"
	ErrNoSuchNetwork   = "no_such_network"
	ErrBadPassword     = "bad_password"
	ErrAlreadyJoined   = "already_joined"
	ErrNetworkFull     = "network_full"
	ErrNotOwner        = "not_owner"
	ErrRateLimited     = "rate_limited"
	ErrInternal        = "internal"
)

// PublicKey is an X25519 public key, base64 in JSON.
type PublicKey [32]byte

func (k PublicKey) MarshalJSON() ([]byte, error) {
	return json.Marshal(base64.StdEncoding.EncodeToString(k[:]))
}

func (k *PublicKey) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	raw, err := base64.StdEncoding.DecodeString(text)
	if err != nil {
		return err
	}
	if len(raw) != 32 {
		return fmt.Errorf("expected a 32-byte key, got %d", len(raw))
	}
	copy(k[:], raw)
	return nil
}

// RelayToken authorises a peer on the UDP plane. Hex in JSON.
type RelayToken [RelayTokenLen]byte

func (t RelayToken) MarshalJSON() ([]byte, error) {
	return json.Marshal(hex.EncodeToString(t[:]))
}

func (t *RelayToken) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	raw, err := hex.DecodeString(text)
	if err != nil {
		return err
	}
	if len(raw) != RelayTokenLen {
		return fmt.Errorf("expected a %d-byte token, got %d", RelayTokenLen, len(raw))
	}
	copy(t[:], raw)
	return nil
}

// PeerInfo is everything needed to open a tunnel to somebody.
type PeerInfo struct {
	PeerID PeerID `json:"peer_id"`
	// Display name the peer chose. Untrusted, never used for addressing.
	Name      string     `json:"name"`
	PublicKey PublicKey  `json:"public_key"`
	VirtualIP netip.Addr `json:"virtual_ip"`
	// Candidate underlay addresses, best guess first.
	Endpoints []netip.AddrPort `json:"endpoints"`
}

// NetworkInfo describes a network from one member's point of view.
type NetworkInfo struct {
	NetworkID string     `json:"network_id"`
	Name      string     `json:"name"`
	VirtualIP netip.Addr `json:"virtual_ip"`
	PrefixLen uint8      `json:"prefix_len"`
}

// ClientMsg is a decoded client -> server frame. Only the fields belonging to
// the variant named by T are meaningful.
type ClientMsg struct {
	T         string
	Version   uint32
	PublicKey PublicKey
	Name      string
	NetworkID string
	Password  string
	Endpoints []netip.AddrPort
}

// UnmarshalClientMsg decodes one frame, checking that the fields the variant
// requires are actually present.
func UnmarshalClientMsg(data []byte) (ClientMsg, error) {
	var probe struct {
		T string `json:"t"`
	}
	if err := json.Unmarshal(data, &probe); err != nil {
		return ClientMsg{}, err
	}

	msg := ClientMsg{T: probe.T}
	switch probe.T {
	case MsgHello:
		var body struct {
			Version   *uint32    `json:"version"`
			PublicKey *PublicKey `json:"public_key"`
			Name      *string    `json:"name"`
		}
		if err := json.Unmarshal(data, &body); err != nil {
			return msg, err
		}
		if body.Version == nil || body.PublicKey == nil || body.Name == nil {
			return msg, fmt.Errorf("hello is missing a field")
		}
		msg.Version, msg.PublicKey, msg.Name = *body.Version, *body.PublicKey, *body.Name

	case MsgCreateNetwork:
		var body struct {
			Name     *string `json:"name"`
			Password *string `json:"password"`
		}
		if err := json.Unmarshal(data, &body); err != nil {
			return msg, err
		}
		if body.Name == nil || body.Password == nil {
			return msg, fmt.Errorf("create_network is missing a field")
		}
		msg.Name, msg.Password = *body.Name, *body.Password

	case MsgJoinNetwork:
		var body struct {
			NetworkID *string `json:"network_id"`
			Password  *string `json:"password"`
		}
		if err := json.Unmarshal(data, &body); err != nil {
			return msg, err
		}
		if body.NetworkID == nil || body.Password == nil {
			return msg, fmt.Errorf("join_network is missing a field")
		}
		msg.NetworkID, msg.Password = *body.NetworkID, *body.Password

	case MsgLeaveNetwork, MsgDeleteNetwork:
		var body struct {
			NetworkID *string `json:"network_id"`
		}
		if err := json.Unmarshal(data, &body); err != nil {
			return msg, err
		}
		if body.NetworkID == nil {
			return msg, fmt.Errorf("%s is missing network_id", probe.T)
		}
		msg.NetworkID = *body.NetworkID

	case MsgUpdateEndpoints:
		var body struct {
			Endpoints []netip.AddrPort `json:"endpoints"`
		}
		if err := json.Unmarshal(data, &body); err != nil {
			return msg, err
		}
		msg.Endpoints = body.Endpoints

	case MsgPing:
		// No fields.

	default:
		return msg, fmt.Errorf("unknown control message %q", probe.T)
	}
	return msg, nil
}

// MarshalServerMsg builds a server -> client frame: the variant's own fields,
// with "t" spliced in beside them.
func MarshalServerMsg(t string, body any) ([]byte, error) {
	fields := map[string]json.RawMessage{}
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return nil, err
		}
		if err := json.Unmarshal(raw, &fields); err != nil {
			return nil, err
		}
	}
	if _, taken := fields["t"]; taken {
		return nil, fmt.Errorf("message body may not carry its own \"t\" field")
	}
	tag, err := json.Marshal(t)
	if err != nil {
		return nil, err
	}
	fields["t"] = tag
	return json.Marshal(fields)
}

// Bodies of the server messages that carry fields.
type (
	WelcomeBody struct {
		Version    uint32     `json:"version"`
		PeerID     PeerID     `json:"peer_id"`
		VirtualIP  netip.Addr `json:"virtual_ip"`
		PrefixLen  uint8      `json:"prefix_len"`
		UDPPort    uint16     `json:"udp_port"`
		RelayToken RelayToken `json:"relay_token"`
	}
	NetworkCreatedBody struct {
		NetworkID string `json:"network_id"`
		Name      string `json:"name"`
	}
	JoinedBody struct {
		Network NetworkInfo `json:"network"`
		Peers   []PeerInfo  `json:"peers"`
	}
	NetworkIDBody struct {
		NetworkID string `json:"network_id"`
	}
	PeerUpdateBody struct {
		NetworkID string   `json:"network_id"`
		Peer      PeerInfo `json:"peer"`
	}
	PeerLeftBody struct {
		NetworkID string `json:"network_id"`
		PeerID    PeerID `json:"peer_id"`
	}
	ErrorBody struct {
		Code    string `json:"code"`
		Message string `json:"message"`
	}
)

// WriteFrame writes one length-prefixed frame.
func WriteFrame(w io.Writer, payload []byte) error {
	if len(payload) > MaxFrameSize {
		return fmt.Errorf("frame of %d bytes exceeds the limit", len(payload))
	}
	var header [4]byte
	binary.BigEndian.PutUint32(header[:], uint32(len(payload)))
	if _, err := w.Write(header[:]); err != nil {
		return err
	}
	_, err := w.Write(payload)
	return err
}

// ReadFrame reads one length-prefixed frame. It returns io.EOF on a clean close.
func ReadFrame(r io.Reader) ([]byte, error) {
	var header [4]byte
	if _, err := io.ReadFull(r, header[:]); err != nil {
		return nil, err
	}
	length := binary.BigEndian.Uint32(header[:])
	if length > MaxFrameSize {
		return nil, fmt.Errorf("frame of %d bytes exceeds the limit", length)
	}
	payload := make([]byte, length)
	if _, err := io.ReadFull(r, payload); err != nil {
		return nil, err
	}
	return payload, nil
}

// GenerateNetworkID produces an ID shaped like HAM-4821-9903: short enough to
// read aloud, large enough that guessing one is impractical when it also needs a
// password.
func GenerateNetworkID(rng *rand.Rand) string {
	return fmt.Sprintf("HAM-%04d-%04d", rng.Intn(10000), rng.Intn(10000))
}
