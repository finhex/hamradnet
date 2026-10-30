package client

import (
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"log/slog"
	"math/rand"
	"net"
	"net/netip"
	"sync"
	"time"

	"github.com/finhex/hamradnet/internal/proto"
	"github.com/finhex/hamradnet/internal/session"
)

const (
	// InterfaceName is the virtual interface this client creates.
	InterfaceName = "hamrad0"
	// tickInterval drives handshakes, keepalives and path probing.
	tickInterval = time.Second
	// serverRefresh is how often we re-register with the relay and re-check our
	// public address.
	serverRefresh = 15 * time.Second
	// controlPing is the control-plane keepalive.
	controlPing = 30 * time.Second
)

// Identity is what the coordinator told us about ourselves.
type Identity struct {
	PeerID     proto.PeerID
	VirtualIP  netip.Addr
	PrefixLen  uint8
	RelayToken proto.RelayToken
	ServerUDP  netip.AddrPort
}

// Event reports the outcome of something the user asked for.
type Event struct {
	Kind      string // "created", "joined", "left", "deleted", "failed"
	NetworkID string
	Name      string
	Message   string
}

// datagram is one packet waiting to go out.
type datagram struct {
	data []byte
	dest netip.AddrPort
}

// via records how an inbound packet reached us. Only a direct arrival may
// confirm a path.
type via struct {
	direct bool
	addr   netip.AddrPort
}

// Daemon owns the tunnel: the interface, the UDP socket, the peer table and the
// connection to the coordinator.
type Daemon struct {
	mu sync.Mutex

	config     *Config
	configPath string
	// Interface name to create. Settable so two clients can run on one host.
	interfaceName string
	peers         *PeerTable
	networks      map[string]proto.NetworkInfo
	// Passwords for joins in flight, kept so a successful join can be saved.
	pendingJoins map[string]string

	identity    *Identity
	connected   bool
	lastError   string
	publicAddr  netip.AddrPort
	hasPublic   bool
	localAddr   netip.AddrPort
	hasLocal    bool
	serverRTT   time.Duration
	reflectID   uint64
	reflectAt   time.Time
	reflectOpen bool
	// The first probe is thrown away: it goes out while the NAT mapping is still
	// being made, so it times the setup rather than the path.
	reflectSamples int

	sock     *net.UDPConn
	tun      *tunDevice
	toTun    chan []byte
	toServer chan []byte
	// Each in-flight command gets its own channel, so two running at once do not
	// steal each other's outcome.
	subscribers []chan Event
	log         *slog.Logger

	// logs keeps recent log lines for the app and `hamradnet logs`; nil when
	// this process does not keep them.
	logs *LogBuffer
	// ipcOwner, when set, is given the local control file, so that user's app
	// and command line can drive a daemon running as root, as a service.
	ipcOwner *[2]int
	// configured wakes the connection loop when a server is chosen or changed.
	configured chan struct{}
	// endSession drops the coordinator connection in progress, and switching
	// marks that as a change of server rather than a failure.
	endSession context.CancelFunc
	switching  bool
}

// noServerYet is what status reports before anyone has chosen a server.
const noServerYet = "no server chosen yet: pick one in the app, or run `hamradnet connect --server HOST`"

// NewDaemon prepares a daemon. Nothing runs until Run is called.
func NewDaemon(config *Config, configPath string, log *slog.Logger) *Daemon {
	return &Daemon{
		config:        config,
		configPath:    configPath,
		interfaceName: InterfaceName,
		peers:         NewPeerTable(),
		networks:      map[string]proto.NetworkInfo{},
		pendingJoins:  map[string]string{},
		toTun:         make(chan []byte, 256),
		toServer:      make(chan []byte, 64),
		log:           log,
		configured:    make(chan struct{}, 1),
	}
}

// SetLogBuffer gives the daemon the buffer its own log is also written to, so
// it can hand the log out over the local control channel.
func (d *Daemon) SetLogBuffer(logs *LogBuffer) { d.logs = logs }

// SetIPCOwner makes the local control file belong to this user, so that
// user's app and command line can drive the daemon without root. Call it before
// ServeIPC.
func (d *Daemon) SetIPCOwner(uid, gid int) { d.ipcOwner = &[2]int{uid, gid} }

// Configure points the daemon at a coordinator, and renames this player if name
// is not empty, from the app or `hamradnet connect`. The choice is saved, and a
// change of server takes effect at once.
func (d *Daemon) Configure(server, name string) error {
	server = NormalizeServer(server)
	if server == "" {
		return errors.New("no server address given")
	}
	d.mu.Lock()
	changed := d.config.Server != server || (name != "" && d.config.Name != name)
	d.config.Server = server
	if name != "" {
		d.config.Name = name
	}
	err := d.config.Save(d.configPath)
	end := d.endSession
	if changed {
		d.lastError = ""
		d.switching = end != nil
	}
	d.mu.Unlock()
	if err != nil {
		return fmt.Errorf("saving the settings: %w", err)
	}
	if changed {
		d.log.Info("server chosen", "server", server, "name", name)
		if end != nil {
			end()
		}
		select {
		case d.configured <- struct{}{}:
		default:
		}
	}
	return nil
}

// SetInterfaceName overrides the interface this client creates. It must be
// called before Run, and exists so two clients can share one host, which is
// what testing the tunnel end to end needs.
func (d *Daemon) SetInterfaceName(name string) {
	if name != "" {
		d.interfaceName = name
	}
}

// emit reports an outcome to every command currently waiting on one.
func (d *Daemon) emit(e Event) {
	d.mu.Lock()
	listeners := append([]chan Event(nil), d.subscribers...)
	d.mu.Unlock()
	for _, ch := range listeners {
		select {
		case ch <- e:
		default:
			// That command is not keeping up; its own timeout will end it.
		}
	}
}

// Run brings the tunnel up and keeps it up until ctx is cancelled.
func (d *Daemon) Run(ctx context.Context, udpPort int) error {
	sock, err := net.ListenUDP("udp4", &net.UDPAddr{Port: udpPort})
	if err != nil {
		return fmt.Errorf("binding the local UDP socket: %w", err)
	}
	d.sock = sock
	defer sock.Close()
	bound := sock.LocalAddr().(*net.UDPAddr)
	d.log.Info("udp socket bound", "addr", bound.String())

	go d.udpLoop(ctx)
	go d.tickLoop(ctx)

	// Reconnect forever; each pass rebuilds the peer table from a fresh roster.
	backoff := time.Second
	for ctx.Err() == nil {
		// Running as a service, the daemon starts before anyone has chosen a
		// server, and waits for the app to choose one.
		d.mu.Lock()
		server := d.config.Server
		if server == "" {
			d.lastError = noServerYet
		}
		d.mu.Unlock()
		if server == "" {
			select {
			case <-ctx.Done():
				return nil
			case <-d.configured:
			}
			continue
		}

		session, endSession := context.WithCancel(ctx)
		d.mu.Lock()
		d.endSession = endSession
		d.mu.Unlock()
		err := d.controlSession(session, uint16(bound.Port))
		endSession()

		d.mu.Lock()
		d.endSession = nil
		switching := d.switching
		d.switching = false
		d.connected = false
		d.networks = map[string]proto.NetworkInfo{}
		d.peers = NewPeerTable()
		if err != nil && !switching {
			d.lastError = err.Error()
		}
		d.mu.Unlock()

		switch {
		case switching:
			d.log.Info("switching to the new server")
			backoff = time.Second
			continue
		case err != nil:
			d.log.Warn("control connection failed", "err", err)
		default:
			// A connection that came up and later dropped is not a reason to
			// keep backing off.
			d.log.Warn("control connection closed")
			backoff = time.Second
		}

		select {
		case <-ctx.Done():
			return nil
		case <-time.After(backoff):
		case <-d.configured:
			backoff = time.Second
			continue
		}
		if backoff < 30*time.Second {
			backoff *= 2
		}
	}
	return nil
}

// Close tears the tunnel down.
func (d *Daemon) Close() {
	d.mu.Lock()
	device := d.tun
	d.tun = nil
	d.mu.Unlock()
	if device != nil {
		device.Close()
	}
	if d.sock != nil {
		_ = d.sock.Close()
	}
}

// ---------------------------------------------------------------- control ---

func (d *Daemon) controlSession(ctx context.Context, localUDPPort uint16) error {
	d.mu.Lock()
	server := d.config.Server
	d.mu.Unlock()
	dialer := net.Dialer{Timeout: 15 * time.Second}
	conn, err := dialer.DialContext(ctx, "tcp", server)
	if err != nil {
		return fmt.Errorf("connecting to %s: %w", server, err)
	}
	defer conn.Close()
	// Ending the session, for a change of server, closes the connection, which
	// is what unblocks the reads below.
	stop := context.AfterFunc(ctx, func() { _ = conn.Close() })
	defer stop()
	if tcp, ok := conn.(*net.TCPConn); ok {
		_ = tcp.SetNoDelay(true)
	}

	// The route the kernel picked to reach the server is also the address peers
	// on the same LAN should use for us.
	if local, ok := proto.ToAddrPort(&net.UDPAddr{
		IP:   conn.LocalAddr().(*net.TCPAddr).IP,
		Port: int(localUDPPort),
	}); ok {
		d.mu.Lock()
		d.localAddr, d.hasLocal = local, true
		d.mu.Unlock()
	}

	publicKey, err := d.config.PublicKey()
	if err != nil {
		return err
	}
	hello, err := proto.MarshalServerMsg(proto.MsgHello, map[string]any{
		"version":    proto.Version,
		"public_key": proto.PublicKey(publicKey),
		"name":       d.config.Name,
	})
	if err != nil {
		return err
	}
	if err := proto.WriteFrame(conn, hello); err != nil {
		return err
	}

	frame, err := proto.ReadFrame(conn)
	if err != nil {
		return err
	}
	welcome, err := parseServerMsg(frame)
	if err != nil {
		return err
	}
	if welcome.T == proto.MsgError {
		return fmt.Errorf("coordinator rejected us: %s", welcome.Message)
	}
	if welcome.T != proto.MsgWelcome {
		return fmt.Errorf("expected welcome, got %q", welcome.T)
	}
	if welcome.Version != proto.Version {
		return fmt.Errorf("coordinator speaks protocol v%d, this build speaks v%d",
			welcome.Version, proto.Version)
	}

	serverIP := conn.RemoteAddr().(*net.TCPAddr).IP
	serverAddr, _ := proto.ToAddrPort(&net.UDPAddr{IP: serverIP, Port: int(welcome.UDPPort)})
	identity := &Identity{
		PeerID:     welcome.PeerID,
		VirtualIP:  welcome.VirtualIP,
		PrefixLen:  welcome.PrefixLen,
		RelayToken: welcome.RelayToken,
		ServerUDP:  serverAddr,
	}
	d.log.Info("connected to coordinator", "peer_id", identity.PeerID, "address", identity.VirtualIP)

	if err := d.ensureTun(ctx, identity); err != nil {
		return err
	}

	d.mu.Lock()
	d.identity = identity
	d.connected = true
	d.lastError = ""
	rejoin := append([]SavedNetwork(nil), d.config.Networks...)
	d.mu.Unlock()

	// Register with the relay before rejoining anything, so it can already
	// deliver by the time the first roster arrives and handshakes start.
	d.announceToRelay()

	for _, saved := range rejoin {
		d.mu.Lock()
		d.pendingJoins[saved.NetworkID] = saved.Password
		d.mu.Unlock()
		d.sendToServer(proto.MsgJoinNetwork, map[string]any{
			"network_id": saved.NetworkID,
			"password":   saved.Password,
		})
	}
	d.publishEndpoints()

	sessionCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	go d.controlWriter(sessionCtx, conn)

	for {
		_ = conn.SetReadDeadline(time.Now().Add(2 * controlPing))
		frame, err := proto.ReadFrame(conn)
		if err != nil {
			return err
		}
		msg, err := parseServerMsg(frame)
		if err != nil {
			return err
		}
		d.handleServerMsg(msg)
	}
}

func (d *Daemon) controlWriter(ctx context.Context, conn net.Conn) {
	ping := time.NewTicker(controlPing)
	defer ping.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case frame := <-d.toServer:
			if err := proto.WriteFrame(conn, frame); err != nil {
				return
			}
		case <-ping.C:
			frame, err := proto.MarshalServerMsg(proto.MsgPing, nil)
			if err != nil {
				continue
			}
			if err := proto.WriteFrame(conn, frame); err != nil {
				return
			}
		}
	}
}

func (d *Daemon) sendToServer(t string, body any) {
	frame, err := proto.MarshalServerMsg(t, body)
	if err != nil {
		d.log.Error("could not encode a control message", "t", t, "err", err)
		return
	}
	select {
	case d.toServer <- frame:
	default:
		d.log.Warn("control queue is full, dropping a message", "t", t)
	}
}

func (d *Daemon) publishEndpoints() {
	d.mu.Lock()
	var endpoints []netip.AddrPort
	if d.hasLocal {
		endpoints = append(endpoints, d.localAddr)
	}
	d.mu.Unlock()
	if len(endpoints) == 0 {
		return
	}
	d.sendToServer(proto.MsgUpdateEndpoints, map[string]any{"endpoints": endpoints})
}

func (d *Daemon) handleServerMsg(msg serverMsg) {
	var out []datagram

	switch msg.T {
	case proto.MsgJoined:
		d.mu.Lock()
		d.log.Info("joined network", "network", msg.Network.Name, "id", msg.Network.NetworkID,
			"peers", len(msg.Peers))
		if password, ok := d.pendingJoins[msg.Network.NetworkID]; ok {
			delete(d.pendingJoins, msg.Network.NetworkID)
			d.config.RememberNetwork(msg.Network.NetworkID, password)
			if err := d.config.Save(d.configPath); err != nil {
				d.log.Warn("could not save the network to the config", "err", err)
			}
		}
		d.networks[msg.Network.NetworkID] = msg.Network
		for _, info := range msg.Peers {
			d.addPeerLocked(info, &out)
			d.peers.AddToNetwork(info.PeerID, msg.Network.NetworkID)
		}
		d.mu.Unlock()
		d.emit(Event{Kind: "joined", NetworkID: msg.Network.NetworkID, Name: msg.Network.Name})

	case proto.MsgPeerUpdate:
		d.mu.Lock()
		d.addPeerLocked(msg.Peer, &out)
		d.peers.AddToNetwork(msg.Peer.PeerID, msg.NetworkID)
		d.mu.Unlock()

	case proto.MsgPeerLeft:
		d.mu.Lock()
		// Only drop the peer once we share no networks with them at all.
		if d.peers.RemoveFromNetwork(msg.PeerID, msg.NetworkID) {
			d.log.Info("peer left", "peer_id", msg.PeerID)
		}
		d.mu.Unlock()

	case proto.MsgNetworkCreated:
		d.log.Info("network created", "network_id", msg.NetworkID, "name", msg.Name)
		d.emit(Event{Kind: "created", NetworkID: msg.NetworkID, Name: msg.Name})

	case proto.MsgLeft:
		d.mu.Lock()
		delete(d.networks, msg.NetworkID)
		d.peers.ForgetNetwork(msg.NetworkID)
		d.config.ForgetNetwork(msg.NetworkID)
		_ = d.config.Save(d.configPath)
		d.mu.Unlock()
		d.emit(Event{Kind: "left", NetworkID: msg.NetworkID})

	case proto.MsgDeleted:
		d.mu.Lock()
		d.log.Info("network deleted", "network_id", msg.NetworkID)
		delete(d.networks, msg.NetworkID)
		d.peers.ForgetNetwork(msg.NetworkID)
		// Stop trying to rejoin something that no longer exists.
		d.config.ForgetNetwork(msg.NetworkID)
		_ = d.config.Save(d.configPath)
		d.mu.Unlock()
		d.emit(Event{Kind: "deleted", NetworkID: msg.NetworkID})

	case proto.MsgError:
		d.log.Warn("coordinator returned an error", "code", msg.Code, "message", msg.Message)
		d.emit(Event{Kind: "failed", Message: msg.Message})
	}

	d.flush(out)
}

// addPeerLocked adds or refreshes a peer and, if it is our turn to dial, starts
// a handshake. The caller holds the mutex.
func (d *Daemon) addPeerLocked(info proto.PeerInfo, out *[]datagram) {
	if d.identity == nil || info.PeerID == d.identity.PeerID {
		return
	}
	_, known := d.peers.Get(info.PeerID)
	d.peers.Upsert(info)
	if !known {
		d.log.Info("peer appeared", "peer_id", info.PeerID, "name", info.Name)
	}
	if ShouldInitiate(d.identity.PeerID, info.PeerID) {
		d.startHandshakeLocked(info.PeerID, out)
	}
	d.punchLocked(info.PeerID, out)
}

// startHandshakeLocked sends the first Noise message to every candidate address
// at once, and through the relay as well, so whichever path works wins the race.
func (d *Daemon) startHandshakeLocked(id proto.PeerID, out *[]datagram) {
	peer, ok := d.peers.Get(id)
	if !ok || d.identity == nil {
		return
	}
	private, err := d.config.PrivateKeyBytes()
	if err != nil {
		return
	}
	localIndex := session.RandomIndex()
	initiator, message, err := session.Initiate(private, peer.Info.PublicKey, localIndex)
	if err != nil {
		d.log.Warn("could not start a handshake", "peer_id", id, "err", err)
		return
	}

	packet := proto.Packet{Type: proto.TypeHandshakeInit, SenderIndex: localIndex, Noise: message}.Encode(nil)
	for _, dest := range peer.Candidates {
		*out = append(*out, datagram{data: append([]byte(nil), packet...), dest: dest})
	}
	*out = append(*out, datagram{
		data: proto.Packet{Type: proto.TypeRelayForward, Dst: id, Inner: packet}.Encode(nil),
		dest: d.identity.ServerUDP,
	})

	peer.Link = LinkHandshaking
	peer.Initiator = initiator
	peer.LastHandshake = time.Now()
	d.peers.BindIndex(id, localIndex)
}

// punchLocked opens an outbound NAT mapping toward every candidate, so the
// peer's own handshake has somewhere to land. Both sides do this regardless of
// who dials.
func (d *Daemon) punchLocked(id proto.PeerID, out *[]datagram) {
	peer, ok := d.peers.Get(id)
	if !ok || d.identity == nil {
		return
	}
	packet := proto.Packet{Type: proto.TypePunch, Sender: d.identity.PeerID}.Encode(nil)
	for _, dest := range peer.Candidates {
		*out = append(*out, datagram{data: append([]byte(nil), packet...), dest: dest})
	}
}

// -------------------------------------------------------------------- udp ---

func (d *Daemon) udpLoop(ctx context.Context) {
	buf := make([]byte, proto.MaxPacketSize)
	for ctx.Err() == nil {
		n, from, err := d.sock.ReadFromUDP(buf)
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				return
			}
			continue
		}
		src, ok := proto.ToAddrPort(from)
		if !ok {
			continue
		}
		d.flush(d.handleDatagram(buf[:n], src))
	}
}

func (d *Daemon) handleDatagram(buf []byte, from netip.AddrPort) []datagram {
	packet, err := proto.Decode(buf)
	if err != nil {
		return nil
	}

	// Unwrap one relay layer, remembering that the path was indirect.
	arrival := via{direct: true, addr: from}
	if packet.Type == proto.TypeRelayDeliver {
		inner, err := proto.Decode(packet.Inner)
		if err != nil {
			return nil
		}
		packet = inner
		arrival = via{direct: false}
	}

	switch packet.Type {
	case proto.TypeReflectResponse:
		d.onReflect(packet.TxID, packet.Observed)
		return nil
	case proto.TypeHandshakeInit:
		return d.onHandshakeInit(packet, arrival)
	case proto.TypeHandshakeResp:
		d.onHandshakeResp(packet, arrival)
		return nil
	case proto.TypeData:
		return d.onData(packet, arrival)
	default:
		// Punch packets have already done their job by being sent.
		return nil
	}
}

func (d *Daemon) onReflect(txid uint64, observed netip.AddrPort) {
	d.mu.Lock()
	if d.reflectOpen && d.reflectID == txid {
		sample := time.Since(d.reflectAt)
		d.reflectOpen = false
		d.reflectSamples++
		if d.reflectSamples > 1 {
			if d.serverRTT == 0 {
				d.serverRTT = sample
			} else {
				d.serverRTT = (d.serverRTT*3 + sample) / 4
			}
		}
	}
	changed := !d.hasPublic || d.publicAddr != observed
	if changed {
		d.log.Info("public address discovered", "observed", observed)
		d.publicAddr, d.hasPublic = observed, true
	}
	d.mu.Unlock()
	if changed {
		d.publishEndpoints()
	}
}

func (d *Daemon) onHandshakeInit(packet proto.Packet, arrival via) []datagram {
	private, err := d.config.PrivateKeyBytes()
	if err != nil {
		return nil
	}
	responder, err := session.Respond(private, packet.Noise)
	if err != nil {
		d.log.Debug("rejected a handshake we could not read", "err", err)
		return nil
	}

	d.mu.Lock()
	defer d.mu.Unlock()
	if d.identity == nil {
		return nil
	}

	// Authorisation: the key must belong to somebody the coordinator says shares
	// a network with us. Anyone else is dropped without a reply.
	id, ok := d.peers.ByPublicKey(proto.PublicKey(responder.RemotePublic))
	if !ok {
		d.log.Debug("handshake from an unknown key, dropping")
		return nil
	}
	peer, _ := d.peers.Get(id)

	// The same initiation arriving over another path: replay the answer already
	// given, so both sides stay on one session rather than building one per path.
	if peer.HasLastResponse && peer.LastResponseTheir == packet.SenderIndex && peer.Link == LinkUp {
		return d.toPathLocked(append([]byte(nil), peer.LastResponse...), id, arrival)
	}

	localIndex := session.RandomIndex()
	sess, reply, err := responder.Finish(localIndex, packet.SenderIndex)
	if err != nil {
		d.log.Warn("could not complete a handshake", "peer_id", id, "err", err)
		return nil
	}
	answer := proto.Packet{
		Type:          proto.TypeHandshakeResp,
		SenderIndex:   localIndex,
		ReceiverIndex: packet.SenderIndex,
		Noise:         reply,
	}.Encode(nil)

	peer.Link = LinkUp
	peer.Session = sess
	peer.Initiator = nil
	peer.LastRX = time.Now()
	peer.LastResponse = append([]byte(nil), answer...)
	peer.LastResponseTheir = packet.SenderIndex
	peer.HasLastResponse = true
	if arrival.direct {
		peer.Confirmed, peer.HasConfirmed = arrival.addr, true
		d.log.Info("tunnel up (direct)", "peer_id", id, "addr", arrival.addr)
	} else {
		peer.RelaySeen = true
		d.log.Info("tunnel up (relayed)", "peer_id", id)
	}
	d.peers.BindIndex(id, localIndex)

	return d.toPathLocked(answer, id, arrival)
}

func (d *Daemon) onHandshakeResp(packet proto.Packet, arrival via) {
	d.mu.Lock()
	defer d.mu.Unlock()

	id, ok := d.peers.BySessionIndex(packet.ReceiverIndex)
	if !ok {
		return
	}
	peer, ok := d.peers.Get(id)
	if !ok {
		return
	}
	// Check before taking: an initiation is raced down several paths, so the
	// answers come back more than once, and tearing the link down on a duplicate
	// would undo the session the first answer just established.
	if peer.Link != LinkHandshaking || peer.Initiator == nil {
		return
	}

	sess, err := peer.Initiator.Finish(packet.Noise, packet.SenderIndex)
	if err != nil {
		d.log.Debug("handshake response rejected", "peer_id", id, "err", err)
		return
	}
	peer.Link = LinkUp
	peer.Session = sess
	peer.Initiator = nil
	peer.LastRX = time.Now()
	if arrival.direct {
		peer.Confirmed, peer.HasConfirmed = arrival.addr, true
		d.log.Info("tunnel up (direct)", "peer_id", id, "addr", arrival.addr)
	} else {
		peer.RelaySeen = true
		d.log.Info("tunnel up (relayed)", "peer_id", id)
	}
}

func (d *Daemon) onData(packet proto.Packet, arrival via) []datagram {
	d.mu.Lock()

	id, ok := d.peers.BySessionIndex(packet.ReceiverIndex)
	if !ok {
		d.mu.Unlock()
		return nil
	}
	peer, ok := d.peers.Get(id)
	if !ok || peer.Session == nil {
		d.mu.Unlock()
		return nil
	}
	plaintext, err := peer.Session.Decrypt(packet.Counter, packet.Ciphertext)
	if err != nil {
		d.mu.Unlock()
		return nil
	}

	peer.LastRX = time.Now()
	peer.RXBytes += uint64(len(plaintext))
	// Only now, with the packet authenticated, is it safe to believe where it
	// came from. Trusting an unverified source address would let a spoofer
	// redirect a peer's traffic.
	if arrival.direct {
		if !peer.HasConfirmed || peer.Confirmed != arrival.addr {
			d.log.Info("direct path confirmed", "peer_id", id, "addr", arrival.addr)
			peer.Confirmed, peer.HasConfirmed = arrival.addr, true
		}
	} else {
		peer.RelaySeen = true
	}

	// An empty payload is a bare keepalive.
	if len(plaintext) == 0 {
		d.mu.Unlock()
		return nil
	}
	// Our own in-tunnel control messages: no IP packet can begin with a zero byte.
	if plaintext[0] == proto.ControlPrefix {
		out := d.handleControlLocked(id, plaintext)
		d.mu.Unlock()
		return out
	}
	d.mu.Unlock()

	select {
	case d.toTun <- plaintext:
	default:
		// The interface is not keeping up; dropping is what a real link does.
	}
	return nil
}

// controlFrame builds [0x00][kind][id: u64 big-endian].
func controlFrame(kind byte, id uint64) []byte {
	out := make([]byte, 2, 10)
	out[0], out[1] = proto.ControlPrefix, kind
	return binary.BigEndian.AppendUint64(out, id)
}

// handleControlLocked answers a latency probe, or records the answer to ours.
func (d *Daemon) handleControlLocked(id proto.PeerID, frame []byte) []datagram {
	if len(frame) < 10 {
		return nil
	}
	probeID := binary.BigEndian.Uint64(frame[2:10])
	peer, ok := d.peers.Get(id)
	if !ok {
		return nil
	}

	switch frame[1] {
	case proto.ControlPing:
		// Echo it straight back; the sender times the round trip.
		return d.encryptForLocked(id, controlFrame(proto.ControlPong, probeID))
	case proto.ControlPong:
		// Only the probe still outstanding counts, so a late duplicate cannot
		// report an inflated time.
		if peer.PingInFlight && peer.PingID == probeID {
			peer.RecordRTT(time.Since(peer.PingSentAt))
			peer.PingInFlight = false
		}
	}
	return nil
}

// encryptForLocked seals a payload for a peer and addresses it to that peer's
// current path.
func (d *Daemon) encryptForLocked(id proto.PeerID, payload []byte) []datagram {
	peer, ok := d.peers.Get(id)
	if !ok || peer.Session == nil || d.identity == nil {
		return nil
	}
	counter, ciphertext, err := peer.Session.Encrypt(payload)
	if err != nil {
		return nil
	}
	packet := proto.Packet{
		Type:          proto.TypeData,
		ReceiverIndex: peer.Session.RemoteIndex,
		Counter:       counter,
		Ciphertext:    ciphertext,
	}.Encode(nil)

	peer.LastTX = time.Now()
	peer.TXBytes += uint64(len(payload))

	if peer.HasConfirmed {
		return []datagram{{data: packet, dest: peer.Confirmed}}
	}
	return []datagram{{
		data: proto.Packet{Type: proto.TypeRelayForward, Dst: id, Inner: packet}.Encode(nil),
		dest: d.identity.ServerUDP,
	}}
}

// toPathLocked queues a packet back along the path it arrived on.
func (d *Daemon) toPathLocked(packet []byte, id proto.PeerID, arrival via) []datagram {
	if arrival.direct {
		return []datagram{{data: packet, dest: arrival.addr}}
	}
	if d.identity == nil {
		return nil
	}
	return []datagram{{
		data: proto.Packet{Type: proto.TypeRelayForward, Dst: id, Inner: packet}.Encode(nil),
		dest: d.identity.ServerUDP,
	}}
}

func (d *Daemon) flush(out []datagram) {
	for _, dg := range out {
		_, _ = d.sock.WriteToUDP(dg.data, net.UDPAddrFromAddrPort(dg.dest))
	}
}

// ------------------------------------------------------------------- tick ---

func (d *Daemon) tickLoop(ctx context.Context) {
	ticker := time.NewTicker(tickInterval)
	defer ticker.Stop()
	sinceRefresh := time.Duration(0)

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
		sinceRefresh += tickInterval
		refresh := sinceRefresh >= serverRefresh
		if refresh {
			sinceRefresh = 0
		}
		d.flush(d.tick(refresh))
	}
}

func (d *Daemon) tick(refreshServer bool) []datagram {
	d.mu.Lock()
	defer d.mu.Unlock()

	if d.identity == nil {
		return nil
	}
	now := time.Now()
	var out []datagram

	if refreshServer {
		out = append(out, d.serverRefreshLocked()...)
	}

	for _, id := range d.peers.IDs() {
		peer, ok := d.peers.Get(id)
		if !ok {
			continue
		}

		if peer.IsStale(now) {
			d.log.Info("peer went quiet, restarting the tunnel", "peer_id", id)
			peer.Link = LinkIdle
			peer.Session = nil
			peer.Initiator = nil
			peer.HasConfirmed = false
			peer.RelaySeen = false
			peer.HasLastResponse = false
			peer.PingInFlight = false
			if ShouldInitiate(d.identity.PeerID, id) {
				d.startHandshakeLocked(id, &out)
			}
			d.punchLocked(id, &out)
			continue
		}

		if peer.NeedsHandshake(now) {
			if ShouldInitiate(d.identity.PeerID, id) {
				d.startHandshakeLocked(id, &out)
			}
			d.punchLocked(id, &out)
		}

		if peer.NeedsKeepalive(now) {
			// An authenticated probe: holds the NAT mapping open and measures
			// the round trip at the same time.
			probeID := rand.Uint64()
			if sent := d.encryptForLocked(id, controlFrame(proto.ControlPing, probeID)); len(sent) > 0 {
				peer.PingID = probeID
				peer.PingSentAt = now
				peer.PingInFlight = true
				out = append(out, sent...)
			}
			if !peer.HasConfirmed {
				out = append(out, d.probeCandidatesLocked(id)...)
			}
		}
	}
	return out
}

// probeCandidatesLocked sends an authenticated keepalive straight at each
// candidate. If one arrives, the peer confirms our address and replies directly,
// and the pair stops paying the relay's latency.
func (d *Daemon) probeCandidatesLocked(id proto.PeerID) []datagram {
	peer, ok := d.peers.Get(id)
	if !ok || peer.Session == nil {
		return nil
	}
	var out []datagram
	for _, dest := range peer.Candidates {
		counter, ciphertext, err := peer.Session.Encrypt(nil)
		if err != nil {
			return out
		}
		out = append(out, datagram{
			data: proto.Packet{
				Type:          proto.TypeData,
				ReceiverIndex: peer.Session.RemoteIndex,
				Counter:       counter,
				Ciphertext:    ciphertext,
			}.Encode(nil),
			dest: dest,
		})
	}
	return out
}

// serverRefreshLocked keeps our relay mapping alive and re-checks our public
// address, timing the round trip while it is at it.
func (d *Daemon) serverRefreshLocked() []datagram {
	txid := rand.Uint64()
	d.reflectID = txid
	d.reflectAt = time.Now()
	d.reflectOpen = true
	return []datagram{
		{
			data: proto.Packet{
				Type:   proto.TypeRelayRegister,
				PeerID: d.identity.PeerID,
				Token:  d.identity.RelayToken,
			}.Encode(nil),
			dest: d.identity.ServerUDP,
		},
		{
			data: proto.Packet{Type: proto.TypeReflectRequest, TxID: txid}.Encode(nil),
			dest: d.identity.ServerUDP,
		},
	}
}

// announceToRelay registers as soon as we have an identity, not just on the
// periodic refresh: until this lands the server has nowhere to deliver relayed
// traffic, so a peer pair that cannot punch would sit unable to connect.
func (d *Daemon) announceToRelay() {
	d.mu.Lock()
	if d.identity == nil {
		d.mu.Unlock()
		return
	}
	out := d.serverRefreshLocked()
	d.mu.Unlock()
	d.flush(out)
}
