package server

import (
	"errors"
	"log/slog"
	"net"

	"github.com/finhex/hamradnet/internal/proto"
)

// ServeRelay runs the UDP plane: endpoint reflection and relay fallback.
//
// The server never sees plaintext here. Relayed payloads are Noise transport
// packets between two clients; the relay rewrites the outer header and forwards.
// Its job is to keep a game working for the pair of players whose NATs refuse to
// hole-punch.
func ServeRelay(conn *net.UDPConn, state *State, log *slog.Logger) error {
	buf := make([]byte, proto.MaxPacketSize)
	out := make([]byte, 0, proto.MaxPacketSize)

	for {
		n, from, err := conn.ReadFromUDP(buf)
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				return nil
			}
			log.Warn("udp recv failed", "err", err)
			continue
		}
		src, ok := proto.ToAddrPort(from)
		if !ok {
			continue
		}

		packet, err := proto.Decode(buf[:n])
		if err != nil {
			// Unsolicited noise on an open UDP port is routine.
			continue
		}

		switch packet.Type {
		case proto.TypeReflectRequest:
			// Lets a client discover its own public address, which is the
			// candidate its peers will actually be able to reach.
			out = proto.Packet{
				Type:     proto.TypeReflectResponse,
				TxID:     packet.TxID,
				Observed: src,
			}.Encode(out)
			_, _ = conn.WriteToUDP(out, from)

		case proto.TypeRelayRegister:
			if state.RegisterRelay(packet.PeerID, src, packet.Token) {
				log.Debug("relay registered", "peer_id", packet.PeerID, "addr", src)
			}

		case proto.TypeRelayForward:
			sender, target, ok := state.RelayTarget(src, packet.Dst)
			if !ok {
				continue
			}
			out = proto.Packet{
				Type:  proto.TypeRelayDeliver,
				Src:   sender,
				Inner: packet.Inner,
			}.Encode(out)
			_, _ = conn.WriteToUDP(out, net.UDPAddrFromAddrPort(target))

		default:
			// Everything else is client-to-client and has no business here.
		}
	}
}
