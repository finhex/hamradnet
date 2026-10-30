package client

import (
	"net/netip"
	"time"

	"github.com/finhex/hamradnet/internal/proto"
	"github.com/finhex/hamradnet/internal/session"
)

const (
	// HandshakeRetry is how long to wait before retrying one that did not land.
	HandshakeRetry = 3 * time.Second
	// KeepaliveInterval holds the NAT mapping open and doubles as the latency
	// probe, so it is frequent enough that the displayed round trip stays
	// current without being wasteful.
	KeepaliveInterval = 5 * time.Second
	// PeerTimeout is how much silence means the path is dead.
	PeerTimeout = 45 * time.Second
)

// LinkState is how far along a peer's tunnel is.
type LinkState int

const (
	// LinkIdle means no tunnel, and none being negotiated.
	LinkIdle LinkState = iota
	// LinkHandshaking means we sent an initiation and are awaiting the reply.
	LinkHandshaking
	// LinkUp means a live tunnel.
	LinkUp
)

func (l LinkState) String() string {
	switch l {
	case LinkUp:
		return "up"
	case LinkHandshaking:
		return "connecting"
	default:
		return "idle"
	}
}

// Peer is somebody in one of our networks.
type Peer struct {
	Info proto.PeerInfo
	// Which of our networks this peer shares with us. A peer can be in more than
	// one, so leaving a single network must not drop them entirely.
	Networks map[string]struct{}

	Link      LinkState
	Initiator *session.Initiator
	Session   *session.Session

	// Addresses worth trying.
	Candidates []netip.AddrPort
	// A candidate confirmed by an authenticated packet actually arriving from it.
	Confirmed    netip.AddrPort
	HasConfirmed bool

	LastRX        time.Time
	LastTX        time.Time
	LastHandshake time.Time
	RelaySeen     bool

	// Smoothed round-trip time, zero until the first reply comes back.
	RTT          time.Duration
	PingID       uint64
	PingSentAt   time.Time
	PingInFlight bool

	// The answer we gave to this peer's last initiation, so the same initiation
	// arriving over another path gets the same answer rather than a new session.
	LastResponse      []byte
	LastResponseTheir uint32
	HasLastResponse   bool

	RXBytes uint64
	TXBytes uint64
}

// IsDirect reports whether traffic goes straight to the peer.
func (p *Peer) IsDirect() bool { return p.HasConfirmed }

// PathName describes the route for display.
func (p *Peer) PathName() string {
	switch {
	case p.Link != LinkUp:
		return "-"
	case p.HasConfirmed:
		return "direct"
	default:
		return "relay"
	}
}

// RecordRTT folds a fresh sample into the smoothed round-trip time. A plain
// average would let one delayed packet dominate for a long time, so new samples
// are weighted at a quarter.
func (p *Peer) RecordRTT(sample time.Duration) {
	if p.RTT == 0 {
		p.RTT = sample
		return
	}
	p.RTT = (p.RTT*3 + sample) / 4
}

// IsStale reports whether the peer has gone quiet for too long.
func (p *Peer) IsStale(now time.Time) bool {
	if !p.LastRX.IsZero() {
		return now.Sub(p.LastRX) > PeerTimeout
	}
	// Never heard from: stale once a handshake has had time to land.
	return !p.LastHandshake.IsZero() && now.Sub(p.LastHandshake) > PeerTimeout
}

// NeedsHandshake reports whether it is time to (re)start one.
func (p *Peer) NeedsHandshake(now time.Time) bool {
	switch p.Link {
	case LinkUp:
		return false
	case LinkHandshaking:
		return p.Initiator == nil || now.Sub(p.Initiator.Started) > HandshakeRetry
	default:
		return true
	}
}

// NeedsKeepalive reports whether a probe is due.
func (p *Peer) NeedsKeepalive(now time.Time) bool {
	return p.Link == LinkUp && (p.LastTX.IsZero() || now.Sub(p.LastTX) > KeepaliveInterval)
}

// PeerTable holds every peer, indexed the several ways packets arrive.
type PeerTable struct {
	peers   map[proto.PeerID]*Peer
	byIP    map[netip.Addr]proto.PeerID
	byKey   map[proto.PublicKey]proto.PeerID
	byIndex map[uint32]proto.PeerID
}

// NewPeerTable makes an empty table.
func NewPeerTable() *PeerTable {
	return &PeerTable{
		peers:   map[proto.PeerID]*Peer{},
		byIP:    map[netip.Addr]proto.PeerID{},
		byKey:   map[proto.PublicKey]proto.PeerID{},
		byIndex: map[uint32]proto.PeerID{},
	}
}

// Upsert adds a peer or refreshes one already known. Candidates from the roster
// are merged rather than replacing what we have, so a path already confirmed is
// never dropped by a roster update.
func (t *PeerTable) Upsert(info proto.PeerInfo) *Peer {
	peer, ok := t.peers[info.PeerID]
	if !ok {
		peer = &Peer{
			Info:       info,
			Networks:   map[string]struct{}{},
			Candidates: append([]netip.AddrPort(nil), info.Endpoints...),
		}
		t.peers[info.PeerID] = peer
	} else {
		if peer.Info.VirtualIP != info.VirtualIP {
			delete(t.byIP, peer.Info.VirtualIP)
		}
		if peer.Info.PublicKey != info.PublicKey {
			delete(t.byKey, peer.Info.PublicKey)
		}
		for _, e := range info.Endpoints {
			if !containsAddr(peer.Candidates, e) {
				peer.Candidates = append(peer.Candidates, e)
			}
		}
		peer.Info = info
	}
	t.byIP[info.VirtualIP] = info.PeerID
	t.byKey[info.PublicKey] = info.PeerID
	return peer
}

func containsAddr(list []netip.AddrPort, want netip.AddrPort) bool {
	for _, e := range list {
		if e == want {
			return true
		}
	}
	return false
}

// Remove drops a peer and every index pointing at it.
func (t *PeerTable) Remove(id proto.PeerID) {
	peer, ok := t.peers[id]
	if !ok {
		return
	}
	delete(t.peers, id)
	delete(t.byIP, peer.Info.VirtualIP)
	delete(t.byKey, peer.Info.PublicKey)
	for index, owner := range t.byIndex {
		if owner == id {
			delete(t.byIndex, index)
		}
	}
}

// AddToNetwork records that a peer is in one of our networks.
func (t *PeerTable) AddToNetwork(id proto.PeerID, networkID string) {
	if peer, ok := t.peers[id]; ok {
		peer.Networks[networkID] = struct{}{}
	}
}

// RemoveFromNetwork removes a peer from one network. The peer itself only goes
// once it has no networks left in common with us.
func (t *PeerTable) RemoveFromNetwork(id proto.PeerID, networkID string) bool {
	peer, ok := t.peers[id]
	if !ok {
		return false
	}
	delete(peer.Networks, networkID)
	if len(peer.Networks) == 0 {
		t.Remove(id)
		return true
	}
	return false
}

// ForgetNetwork drops any peer only reachable through the given network.
func (t *PeerTable) ForgetNetwork(networkID string) {
	var affected []proto.PeerID
	for id, peer := range t.peers {
		if _, in := peer.Networks[networkID]; in {
			affected = append(affected, id)
		}
	}
	for _, id := range affected {
		t.RemoveFromNetwork(id, networkID)
	}
}

// Get looks a peer up by ID.
func (t *PeerTable) Get(id proto.PeerID) (*Peer, bool) {
	peer, ok := t.peers[id]
	return peer, ok
}

// ByVirtualIP resolves a tunnel address to its peer.
func (t *PeerTable) ByVirtualIP(ip netip.Addr) (proto.PeerID, bool) {
	id, ok := t.byIP[ip]
	return id, ok
}

// ByPublicKey resolves an identity key to its peer.
func (t *PeerTable) ByPublicKey(key proto.PublicKey) (proto.PeerID, bool) {
	id, ok := t.byKey[key]
	return id, ok
}

// BySessionIndex resolves one of our own session indexes to its peer.
func (t *PeerTable) BySessionIndex(index uint32) (proto.PeerID, bool) {
	id, ok := t.byIndex[index]
	return id, ok
}

// BindIndex claims a local session index for a peer, replacing any it held.
func (t *PeerTable) BindIndex(id proto.PeerID, index uint32) {
	for existing, owner := range t.byIndex {
		if owner == id {
			delete(t.byIndex, existing)
		}
	}
	t.byIndex[index] = id
}

// IDs lists every peer.
func (t *PeerTable) IDs() []proto.PeerID {
	out := make([]proto.PeerID, 0, len(t.peers))
	for id := range t.peers {
		out = append(out, id)
	}
	return out
}

// Len reports how many peers are known.
func (t *PeerTable) Len() int { return len(t.peers) }

// ShouldInitiate decides which side dials, so two peers do not handshake at each
// other at once and build two half-used sessions. Lower ID calls.
func ShouldInitiate(local, remote proto.PeerID) bool { return local < remote }
