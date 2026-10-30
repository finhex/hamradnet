package server

import (
	"errors"
	"io"
	"log/slog"
	"net"
	"net/netip"
	"strings"
	"time"
	"unicode"

	"github.com/finhex/hamradnet/internal/proto"
)

// idleTimeout drops a client that says nothing at all for this long. Clients
// ping every 30s, so this tolerates two missed pings.
const idleTimeout = 90 * time.Second

// helloTimeout is how long a fresh connection has to introduce itself.
const helloTimeout = 10 * time.Second

// ServeControl accepts control connections until the listener is closed.
func ServeControl(ln net.Listener, state *State, udpPort uint16, log *slog.Logger) error {
	for {
		conn, err := ln.Accept()
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				return nil
			}
			log.Warn("accept failed", "err", err)
			continue
		}
		go func() {
			if err := handleConnection(conn, state, udpPort, log); err != nil {
				log.Debug("connection closed", "addr", conn.RemoteAddr(), "err", err)
			}
		}()
	}
}

func handleConnection(conn net.Conn, state *State, udpPort uint16, log *slog.Logger) error {
	defer conn.Close()
	if tcp, ok := conn.(*net.TCPConn); ok {
		_ = tcp.SetNoDelay(true)
	}

	// The first frame must be hello, and it must arrive promptly.
	_ = conn.SetReadDeadline(time.Now().Add(helloTimeout))
	frame, err := proto.ReadFrame(conn)
	if err != nil {
		return err
	}
	hello, err := proto.UnmarshalClientMsg(frame)
	if err != nil {
		return err
	}
	if hello.T != proto.MsgHello {
		return errors.New("expected hello as the first frame")
	}

	if hello.Version != proto.Version {
		body := proto.ErrorBody{
			Code: proto.ErrVersionMismatch,
			Message: "server speaks protocol v" + itoa(int(proto.Version)) +
				", client sent v" + itoa(int(hello.Version)),
		}
		if out, err := proto.MarshalServerMsg(proto.MsgError, body); err == nil {
			_ = proto.WriteFrame(conn, out)
		}
		return errors.New("version mismatch")
	}

	name := sanitiseName(hello.Name)
	peer, ok := state.RegisterPeer(name, hello.PublicKey)
	if !ok {
		body := proto.ErrorBody{Code: proto.ErrInternal, Message: "no virtual addresses left"}
		if out, err := proto.MarshalServerMsg(proto.MsgError, body); err == nil {
			_ = proto.WriteFrame(conn, out)
		}
		return errors.New("address pool exhausted")
	}
	defer state.RemovePeer(peer.ID)

	log.Info("peer connected", "peer_id", peer.ID, "name", name, "addr", conn.RemoteAddr())

	welcome, err := proto.MarshalServerMsg(proto.MsgWelcome, proto.WelcomeBody{
		Version:    proto.Version,
		PeerID:     peer.ID,
		VirtualIP:  peer.VirtualIP,
		PrefixLen:  proto.VirtualPrefixLen,
		UDPPort:    udpPort,
		RelayToken: peer.RelayToken,
	})
	if err != nil {
		return err
	}
	if err := proto.WriteFrame(conn, welcome); err != nil {
		return err
	}

	// One goroutine owns the write side and drains the notification channel, so
	// anything holding the state lock can queue a message without blocking.
	//
	// It stops on `done` as well as on the channel closing. Waiting only for the
	// close would deadlock: it is RemovePeer that closes peer.Out, RemovePeer
	// runs after this goroutine is joined, and a writer parked on an empty
	// channel would therefore wait forever for a close that waits for it. The
	// peer then stays registered after disconnecting, so nobody is ever told it
	// left and its network never empties.
	done := make(chan struct{})
	writerDone := make(chan struct{})
	go func() {
		defer close(writerDone)
		for {
			select {
			case frame, ok := <-peer.Out:
				if !ok {
					return
				}
				if err := proto.WriteFrame(conn, frame); err != nil {
					return
				}
			case <-done:
				return
			}
		}
	}()

	err = readLoop(conn, state, peer, log)

	// Release the writer before the deferred RemovePeer runs.
	close(done)
	_ = conn.Close()
	<-writerDone
	return err
}

func readLoop(conn net.Conn, state *State, peer *Peer, log *slog.Logger) error {
	for {
		_ = conn.SetReadDeadline(time.Now().Add(idleTimeout))
		frame, err := proto.ReadFrame(conn)
		if err != nil {
			if errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) {
				return nil
			}
			return err
		}
		msg, err := proto.UnmarshalClientMsg(frame)
		if err != nil {
			return err
		}

		switch msg.T {
		case proto.MsgHello:
			return errors.New("duplicate hello")

		case proto.MsgCreateNetwork:
			if msg.Password == "" {
				state.Notify(peer.ID, proto.MsgError, proto.ErrorBody{
					Code: proto.ErrBadPassword, Message: "password required",
				})
				continue
			}
			name := sanitiseName(msg.Name)
			id := state.CreateNetwork(name, msg.Password, peer.PublicKey)
			log.Info("network created", "peer_id", peer.ID, "network_id", id, "name", name)
			state.Notify(peer.ID, proto.MsgNetworkCreated, proto.NetworkCreatedBody{
				NetworkID: id, Name: name,
			})

		case proto.MsgJoinNetwork:
			network, roster, code := state.JoinNetwork(peer.ID, msg.NetworkID, msg.Password)
			if code != "" {
				state.Notify(peer.ID, proto.MsgError, proto.ErrorBody{
					Code: code, Message: describe(code),
				})
				continue
			}
			if roster == nil {
				roster = []proto.PeerInfo{}
			}
			state.Notify(peer.ID, proto.MsgJoined, proto.JoinedBody{
				Network: network, Peers: roster,
			})

		case proto.MsgLeaveNetwork:
			state.LeaveNetwork(peer.ID, msg.NetworkID)
			state.Notify(peer.ID, proto.MsgLeft, proto.NetworkIDBody{NetworkID: msg.NetworkID})

		case proto.MsgDeleteNetwork:
			if code := state.DeleteNetwork(peer.ID, msg.NetworkID); code != "" {
				state.Notify(peer.ID, proto.MsgError, proto.ErrorBody{
					Code: code, Message: describe(code),
				})
			}
			// On success every member, this peer included, is told by the state
			// layer, so there is nothing to send here.

		case proto.MsgUpdateEndpoints:
			endpoints := msg.Endpoints
			if len(endpoints) > 8 {
				endpoints = endpoints[:8]
			}
			state.UpdateEndpoints(peer.ID, endpoints, netip.AddrPort{}, false)

		case proto.MsgPing:
			state.Notify(peer.ID, proto.MsgPong, nil)
		}
	}
}

func describe(code string) string {
	switch code {
	case proto.ErrNoSuchNetwork:
		return "no network with that ID"
	case proto.ErrBadPassword:
		return "wrong password"
	case proto.ErrAlreadyJoined:
		return "already a member of that network"
	case proto.ErrNetworkFull:
		return "that network is full"
	case proto.ErrNotOwner:
		return "only whoever created that network can delete it"
	case proto.ErrVersionMismatch:
		return "protocol version mismatch"
	case proto.ErrRateLimited:
		return "slow down"
	default:
		return "internal error"
	}
}

// sanitiseName strips control characters and caps the length: names are shown in
// other people's peer lists, so whatever the client sent is not to be trusted.
func sanitiseName(raw string) string {
	var b strings.Builder
	count := 0
	for _, r := range raw {
		if unicode.IsControl(r) {
			continue
		}
		if count >= 32 {
			break
		}
		b.WriteRune(r)
		count++
	}
	trimmed := strings.TrimSpace(b.String())
	if trimmed == "" {
		return "anonymous"
	}
	return trimmed
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	var digits []byte
	for n > 0 {
		digits = append([]byte{byte('0' + n%10)}, digits...)
		n /= 10
	}
	return string(digits)
}
