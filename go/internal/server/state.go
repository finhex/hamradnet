// Package server is the HamRadNet coordinator: who is connected, which networks
// exist, who is in them, and relaying for peers that cannot reach each other.
package server

import (
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"log/slog"
	"math/big"
	mathrand "math/rand"
	"net/netip"
	"os"
	"sync"
	"time"

	"github.com/finhex/hamradnet/internal/proto"
)

// MaxPeersPerNetwork caps a single network. Broadcast traffic is copied to every
// member, so an unbounded network would be an amplification vector.
const MaxPeersPerNetwork = 256

// AbandonedAfter is how long a network with nobody in it is kept.
//
// Closing the app is not the same as abandoning a network, so a network outlives
// its members going offline: the same ID works again next weekend. Only when
// nobody has been in it for a fortnight is it removed.
const AbandonedAfter = 14 * 24 * time.Hour

// Peer is a connected client.
type Peer struct {
	ID        proto.PeerID
	Name      string
	PublicKey proto.PublicKey
	VirtualIP netip.Addr
	// Candidates the client reported itself, i.e. its LAN addresses.
	Reported []netip.AddrPort
	// The source address we saw on the UDP plane: the peer's address after NAT,
	// and far more useful than anything it can report about itself.
	Observed    netip.AddrPort
	HasObserved bool
	Networks    map[string]struct{}
	RelayToken  proto.RelayToken
	Out         chan []byte
}

// Candidates lists the addresses worth trying, observed first, since a direct
// path through NAT is the one most likely to work.
func (p *Peer) Candidates() []netip.AddrPort {
	out := make([]netip.AddrPort, 0, len(p.Reported)+1)
	if p.HasObserved {
		out = append(out, p.Observed)
	}
	for _, e := range p.Reported {
		if !contains(out, e) {
			out = append(out, e)
		}
	}
	return out
}

func contains(list []netip.AddrPort, want netip.AddrPort) bool {
	for _, e := range list {
		if e == want {
			return true
		}
	}
	return false
}

// Info renders the peer for the roster sent to others.
func (p *Peer) Info() proto.PeerInfo {
	return proto.PeerInfo{
		PeerID:    p.ID,
		Name:      p.Name,
		PublicKey: p.PublicKey,
		VirtualIP: p.VirtualIP,
		Endpoints: p.Candidates(),
	}
}

// Network is a virtual LAN. The persisted fields are the ones in the JSON tags;
// membership is runtime only.
type Network struct {
	ID           string `json:"id"`
	Name         string `json:"name"`
	Salt         string `json:"salt"`
	PasswordHash string `json:"password_hash"`
	// Public key of whoever created it, hex. Empty for networks created before
	// ownership was recorded, which any member may delete.
	Owner string `json:"owner"`
	// Unix seconds when somebody last joined. Drives expiry.
	LastActive int64 `json:"last_active"`
	// Whether anyone has ever been a member. A network is memberless for the
	// moment between being created and its creator joining, and removing it in
	// that window would be a race the client could never win.
	EverJoined bool `json:"ever_joined"`

	members    map[proto.PeerID]struct{}
	emptySince time.Time
}

func (n *Network) hasOwner() bool { return n.Owner != "" }

func (n *Network) passwordMatches(password string) bool {
	salt, err := hex.DecodeString(n.Salt)
	if err != nil {
		return false
	}
	want, err := hex.DecodeString(n.PasswordHash)
	if err != nil {
		return false
	}
	got := hashPassword(salt, password)
	return subtle.ConstantTimeCompare(got, want) == 1
}

// Salted SHA-256. This guards a game-night network ID, not a bank account: the
// threat is a passer-by who learned an ID, and the hash is never exposed to an
// offline attacker unless the server itself is breached.
func hashPassword(salt []byte, password string) []byte {
	h := sha256.New()
	h.Write(salt)
	h.Write([]byte(password))
	return h.Sum(nil)
}

// ipAllocator hands out addresses inside 10.66.0.0/16, skipping .0 (network),
// .1 (reserved) and the broadcast address.
type ipAllocator struct {
	next     uint32
	released []uint32
}

// lastHost is 10.66.255.254; .255 is the broadcast address.
const lastHost = 0xFFFE

func newIPAllocator() *ipAllocator { return &ipAllocator{next: 2} }

func (a *ipAllocator) allocate() (netip.Addr, bool) {
	var host uint32
	if n := len(a.released); n > 0 {
		host = a.released[n-1]
		a.released = a.released[:n-1]
	} else {
		if a.next > lastHost {
			return netip.Addr{}, false
		}
		host = a.next
		a.next++
	}
	return netip.AddrFrom4([4]byte{10, 66, byte(host >> 8), byte(host & 0xFF)}), true
}

func (a *ipAllocator) release(addr netip.Addr) {
	v4 := addr.As4()
	a.released = append(a.released, uint32(v4[2])<<8|uint32(v4[3]))
}

// State is everything the coordinator knows. One mutex guards all of it;
// critical sections are short, and notifications go out through buffered
// channels so sending never blocks under the lock.
type State struct {
	mu          sync.Mutex
	Peers       map[proto.PeerID]*Peer
	Networks    map[string]*Network
	relayAddrs  map[proto.PeerID]netip.AddrPort
	relayByAddr map[netip.AddrPort]proto.PeerID
	nextPeerID  proto.PeerID
	ips         *ipAllocator
	storePath   string
	emptyGrace  time.Duration
	rng         *mathrand.Rand
	log         *slog.Logger
}

// NewState loads any persisted networks and returns the coordinator state.
func NewState(storePath string, emptyGrace time.Duration, log *slog.Logger) *State {
	s := &State{
		Peers:       map[proto.PeerID]*Peer{},
		Networks:    map[string]*Network{},
		relayAddrs:  map[proto.PeerID]netip.AddrPort{},
		relayByAddr: map[netip.AddrPort]proto.PeerID{},
		nextPeerID:  1,
		ips:         newIPAllocator(),
		storePath:   storePath,
		emptyGrace:  emptyGrace,
		rng:         mathrand.New(mathrand.NewSource(time.Now().UnixNano())),
		log:         log,
	}
	if storePath != "" {
		if err := s.load(); err != nil {
			s.log.Warn("could not load the network store, starting empty", "err", err)
		}
	}
	return s
}

func (s *State) load() error {
	raw, err := os.ReadFile(s.storePath)
	if os.IsNotExist(err) {
		return nil
	}
	if err != nil {
		return err
	}
	var networks []*Network
	if err := json.Unmarshal(raw, &networks); err != nil {
		return err
	}
	now := time.Now()
	for _, n := range networks {
		n.members = map[proto.PeerID]struct{}{}
		if n.LastActive == 0 {
			n.LastActive = now.Unix()
		}
		// Nobody is connected yet after a restart, so an occupied network starts
		// its empty clock here and is reprieved as soon as someone reconnects.
		if n.EverJoined {
			n.emptySince = now
		}
		s.Networks[n.ID] = n
	}
	s.log.Info("loaded persisted networks", "count", len(networks))
	return nil
}

func (s *State) persistLocked() {
	if s.storePath == "" {
		return
	}
	networks := make([]*Network, 0, len(s.Networks))
	for _, n := range s.Networks {
		networks = append(networks, n)
	}
	raw, err := json.MarshalIndent(networks, "", "  ")
	if err != nil {
		s.log.Warn("could not serialise networks", "err", err)
		return
	}
	// Write to a temporary file and rename, so a crash mid-write cannot leave a
	// truncated store behind.
	tmp := s.storePath + ".tmp"
	if err := os.WriteFile(tmp, raw, 0o600); err != nil {
		s.log.Warn("could not persist networks", "err", err)
		return
	}
	if err := os.Rename(tmp, s.storePath); err != nil {
		s.log.Warn("could not replace the network store", "err", err)
	}
}

// RegisterPeer admits a client and allocates its address. The returned channel
// carries frames to send to it.
func (s *State) RegisterPeer(name string, key proto.PublicKey) (*Peer, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()

	ip, ok := s.ips.allocate()
	if !ok {
		return nil, false
	}
	var token proto.RelayToken
	if _, err := rand.Read(token[:]); err != nil {
		s.ips.release(ip)
		return nil, false
	}

	peer := &Peer{
		ID:         s.nextPeerID,
		Name:       name,
		PublicKey:  key,
		VirtualIP:  ip,
		Networks:   map[string]struct{}{},
		RelayToken: token,
		// Buffered so a notification never blocks the sender under the lock. A
		// peer too slow to drain this is disconnected rather than allowed to
		// stall everyone else.
		Out: make(chan []byte, 64),
	}
	s.nextPeerID++
	if s.nextPeerID == 0 {
		s.nextPeerID = 1
	}
	s.Peers[peer.ID] = peer
	return peer, true
}

// RemovePeer drops a peer and tells every network it belonged to.
func (s *State) RemovePeer(id proto.PeerID) {
	s.mu.Lock()
	defer s.mu.Unlock()

	peer, ok := s.Peers[id]
	if !ok {
		return
	}
	delete(s.Peers, id)
	s.ips.release(peer.VirtualIP)
	if addr, ok := s.relayAddrs[id]; ok {
		delete(s.relayByAddr, addr)
		delete(s.relayAddrs, id)
	}

	for networkID := range peer.Networks {
		n, ok := s.Networks[networkID]
		if !ok {
			continue
		}
		delete(n.members, id)
		for member := range n.members {
			s.notifyLocked(member, proto.MsgPeerLeft, proto.PeerLeftBody{
				NetworkID: networkID, PeerID: id,
			})
		}
		// Dropping off counts as leaving, so the last one out closes the network.
		s.markIfEmptyLocked(networkID)
	}
	close(peer.Out)
	s.log.Info("peer disconnected", "peer_id", id, "name", peer.Name)
}

// CreateNetwork makes a network owned by the given key.
func (s *State) CreateNetwork(name, password string, owner proto.PublicKey) string {
	s.mu.Lock()
	defer s.mu.Unlock()

	salt := make([]byte, 16)
	_, _ = rand.Read(salt)

	id := s.uniqueNetworkIDLocked()
	s.Networks[id] = &Network{
		ID:           id,
		Name:         name,
		Salt:         hex.EncodeToString(salt),
		PasswordHash: hex.EncodeToString(hashPassword(salt, password)),
		Owner:        hex.EncodeToString(owner[:]),
		LastActive:   time.Now().Unix(),
		members:      map[proto.PeerID]struct{}{},
	}
	s.persistLocked()
	return id
}

// uniqueNetworkIDLocked draws IDs until it finds one not already taken, so two
// networks can never collide however unlikely that is.
func (s *State) uniqueNetworkIDLocked() string {
	for attempt := 0; attempt < 1000; attempt++ {
		id := proto.GenerateNetworkID(s.rng)
		if _, taken := s.Networks[id]; !taken {
			return id
		}
	}
	// Vanishingly unlikely; fall back to something that cannot collide.
	n, _ := rand.Int(rand.Reader, big.NewInt(1<<62))
	return fmt.Sprintf("HAM-%d", n)
}

// JoinNetwork admits a peer, returning the network and the roster of everyone
// already in it.
func (s *State) JoinNetwork(id proto.PeerID, networkID, password string) (proto.NetworkInfo, []proto.PeerInfo, string) {
	s.mu.Lock()
	defer s.mu.Unlock()

	n, ok := s.Networks[networkID]
	if !ok {
		return proto.NetworkInfo{}, nil, proto.ErrNoSuchNetwork
	}
	if !n.passwordMatches(password) {
		return proto.NetworkInfo{}, nil, proto.ErrBadPassword
	}
	if _, already := n.members[id]; already {
		return proto.NetworkInfo{}, nil, proto.ErrAlreadyJoined
	}
	if len(n.members) >= MaxPeersPerNetwork {
		return proto.NetworkInfo{}, nil, proto.ErrNetworkFull
	}
	peer, ok := s.Peers[id]
	if !ok {
		return proto.NetworkInfo{}, nil, proto.ErrInternal
	}

	existing := make([]proto.PeerID, 0, len(n.members))
	for member := range n.members {
		existing = append(existing, member)
	}

	peer.Networks[networkID] = struct{}{}
	n.members[id] = struct{}{}
	// Any join resets the expiry clock and cancels a pending removal.
	n.LastActive = time.Now().Unix()
	n.EverJoined = true
	n.emptySince = time.Time{}
	s.persistLocked()

	joiner := peer.Info()
	roster := make([]proto.PeerInfo, 0, len(existing))
	for _, member := range existing {
		s.notifyLocked(member, proto.MsgPeerUpdate, proto.PeerUpdateBody{
			NetworkID: networkID, Peer: joiner,
		})
		if other, ok := s.Peers[member]; ok {
			roster = append(roster, other.Info())
		}
	}

	s.log.Info("peer joined network", "peer_id", id, "network_id", networkID, "name", n.Name)
	return proto.NetworkInfo{
		NetworkID: networkID,
		Name:      n.Name,
		VirtualIP: peer.VirtualIP,
		PrefixLen: proto.VirtualPrefixLen,
	}, roster, ""
}

// LeaveNetwork removes a peer from one network, leaving it standing for others.
func (s *State) LeaveNetwork(id proto.PeerID, networkID string) {
	s.mu.Lock()
	defer s.mu.Unlock()

	if peer, ok := s.Peers[id]; ok {
		delete(peer.Networks, networkID)
	}
	n, ok := s.Networks[networkID]
	if !ok {
		return
	}
	delete(n.members, id)
	for member := range n.members {
		s.notifyLocked(member, proto.MsgPeerLeft, proto.PeerLeftBody{
			NetworkID: networkID, PeerID: id,
		})
	}
	s.markIfEmptyLocked(networkID)
}

// DeleteNetwork removes a network for everyone, if the asker is entitled to.
func (s *State) DeleteNetwork(id proto.PeerID, networkID string) string {
	s.mu.Lock()
	defer s.mu.Unlock()

	peer, ok := s.Peers[id]
	if !ok {
		return proto.ErrInternal
	}
	n, ok := s.Networks[networkID]
	if !ok {
		return proto.ErrNoSuchNetwork
	}

	if n.hasOwner() {
		if n.Owner != hex.EncodeToString(peer.PublicKey[:]) {
			return proto.ErrNotOwner
		}
	} else if _, member := n.members[id]; !member {
		// No owner on record, so fall back to requiring membership, which means
		// having produced the password.
		return proto.ErrNotOwner
	}

	members := make([]proto.PeerID, 0, len(n.members))
	for member := range n.members {
		members = append(members, member)
	}
	delete(s.Networks, networkID)
	for _, member := range members {
		if other, ok := s.Peers[member]; ok {
			delete(other.Networks, networkID)
		}
		s.notifyLocked(member, proto.MsgDeleted, proto.NetworkIDBody{NetworkID: networkID})
	}
	s.persistLocked()
	s.log.Info("network deleted", "peer_id", id, "network_id", networkID)
	return ""
}

// markIfEmptyLocked starts the removal clock when the last member goes, and
// removes the network at once when no grace period is configured.
func (s *State) markIfEmptyLocked(networkID string) {
	n, ok := s.Networks[networkID]
	if !ok || len(n.members) > 0 || !n.EverJoined {
		return
	}
	n.emptySince = time.Now()
	if s.emptyGrace == 0 {
		s.log.Info("everyone left, removing the network", "network_id", networkID)
		delete(s.Networks, networkID)
		s.persistLocked()
	}
}

// SweepEmpty removes networks nobody is in once the grace period has passed, and
// networks that were created but never joined once they are truly abandoned.
func (s *State) SweepEmpty() int {
	s.mu.Lock()
	defer s.mu.Unlock()

	now := time.Now()
	var doomed []string
	for id, n := range s.Networks {
		if len(n.members) > 0 {
			continue
		}
		if !n.emptySince.IsZero() {
			if now.Sub(n.emptySince) >= s.emptyGrace {
				doomed = append(doomed, id)
			}
			continue
		}
		// Never occupied: only the abandonment rule applies, so that creating a
		// network and joining it is not a race.
		if !n.EverJoined && now.Sub(time.Unix(n.LastActive, 0)) >= AbandonedAfter {
			doomed = append(doomed, id)
		}
	}
	for _, id := range doomed {
		s.log.Info("removing an empty network", "network_id", id)
		delete(s.Networks, id)
	}
	if len(doomed) > 0 {
		s.persistLocked()
	}
	return len(doomed)
}

// UpdateEndpoints records new candidate addresses and republishes them to the
// peer's networks. It reports whether anything actually changed.
func (s *State) UpdateEndpoints(id proto.PeerID, reported []netip.AddrPort, observed netip.AddrPort, hasObserved bool) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.updateEndpointsLocked(id, reported, observed, hasObserved)
}

func (s *State) updateEndpointsLocked(id proto.PeerID, reported []netip.AddrPort, observed netip.AddrPort, hasObserved bool) bool {
	peer, ok := s.Peers[id]
	if !ok {
		return false
	}
	changed := false
	if reported != nil && !sameAddrs(peer.Reported, reported) {
		peer.Reported = reported
		changed = true
	}
	if hasObserved && (!peer.HasObserved || peer.Observed != observed) {
		peer.Observed = observed
		peer.HasObserved = true
		changed = true
	}
	if !changed {
		return false
	}

	info := peer.Info()
	for networkID := range peer.Networks {
		n, ok := s.Networks[networkID]
		if !ok {
			continue
		}
		for member := range n.members {
			if member == id {
				continue
			}
			s.notifyLocked(member, proto.MsgPeerUpdate, proto.PeerUpdateBody{
				NetworkID: networkID, Peer: info,
			})
		}
	}
	return true
}

func sameAddrs(a, b []netip.AddrPort) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// RegisterRelay binds a UDP source address to a peer, but only when the datagram
// carried the token that peer was issued over its authenticated control
// connection. Without this check anyone could claim any peer's ID and have its
// relayed traffic delivered to them.
func (s *State) RegisterRelay(id proto.PeerID, addr netip.AddrPort, token proto.RelayToken) bool {
	s.mu.Lock()
	defer s.mu.Unlock()

	peer, ok := s.Peers[id]
	if !ok {
		return false
	}
	if subtle.ConstantTimeCompare(peer.RelayToken[:], token[:]) != 1 {
		s.log.Warn("relay registration with a bad token", "peer_id", id, "addr", addr)
		return false
	}
	if previous, ok := s.relayAddrs[id]; ok && previous != addr {
		delete(s.relayByAddr, previous)
	}
	s.relayAddrs[id] = addr
	s.relayByAddr[addr] = id
	// The address we just saw is also the best endpoint to publish.
	s.updateEndpointsLocked(id, nil, addr, true)
	return true
}

// RelayTarget resolves where to forward a relayed packet, refusing peers with no
// network in common.
func (s *State) RelayTarget(from netip.AddrPort, dst proto.PeerID) (proto.PeerID, netip.AddrPort, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()

	sender, ok := s.relayByAddr[from]
	if !ok {
		return 0, netip.AddrPort{}, false
	}
	if !s.sharesNetworkLocked(sender, dst) {
		return 0, netip.AddrPort{}, false
	}
	addr, ok := s.relayAddrs[dst]
	if !ok {
		return 0, netip.AddrPort{}, false
	}
	return sender, addr, true
}

func (s *State) sharesNetworkLocked(a, b proto.PeerID) bool {
	x, ok := s.Peers[a]
	if !ok {
		return false
	}
	y, ok := s.Peers[b]
	if !ok {
		return false
	}
	for id := range x.Networks {
		if _, both := y.Networks[id]; both {
			return true
		}
	}
	return false
}

// Notify queues a message for one peer.
func (s *State) Notify(id proto.PeerID, t string, body any) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.notifyLocked(id, t, body)
}

// notifyLocked queues a message. A full channel means the peer is not keeping
// up; the frame is dropped rather than stalling every other peer behind it.
func (s *State) notifyLocked(id proto.PeerID, t string, body any) {
	peer, ok := s.Peers[id]
	if !ok {
		return
	}
	frame, err := proto.MarshalServerMsg(t, body)
	if err != nil {
		s.log.Error("could not encode a message", "t", t, "err", err)
		return
	}
	select {
	case peer.Out <- frame:
	default:
		s.log.Warn("dropping a message for a peer that is not keeping up", "peer_id", id)
	}
}

// PeerByID looks up a connected peer.
func (s *State) PeerByID(id proto.PeerID) (*Peer, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	peer, ok := s.Peers[id]
	return peer, ok
}

// NetworkCount reports how many networks exist, for tests and diagnostics.
func (s *State) NetworkCount() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return len(s.Networks)
}

// HasNetwork reports whether a network exists.
func (s *State) HasNetwork(id string) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	_, ok := s.Networks[id]
	return ok
}
