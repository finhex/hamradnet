// Package proto is the HamRadNet wire protocol.
//
// It is byte-compatible with the Rust implementation in rust/crates/proto, so a
// Go client can talk to a Rust coordinator and the other way round. Anything
// changed here has to be changed there too.
package proto

import (
	"encoding/binary"
	"errors"
	"fmt"
	"net"
	"net/netip"
)

// Version of the control and data planes. Bumped only on an incompatible change.
const Version uint32 = 1

const (
	DefaultControlPort = 7777
	DefaultUDPPort     = 7778
)

// Virtual subnet handed out to peers: 10.66.0.0/16.
var (
	VirtualSubnet    = netip.AddrFrom4([4]byte{10, 66, 0, 0})
	VirtualBroadcast = netip.AddrFrom4([4]byte{10, 66, 255, 255})
)

const VirtualPrefixLen = 16

// MTU of the virtual interface.
//
// 1280 is the IPv6 minimum every path must carry, so a packet this size gets
// through anything. It must match on every machine: a client sending packets
// larger than another client's interface accepts means traffic is silently
// dropped at the far end.
const TunMTU = 1280

// Payloads inside the tunnel are normally IP packets, whose first byte carries
// the version in the high nibble and so is always >= 0x40. A leading zero byte
// marks one of our own in-tunnel control messages instead.
const (
	ControlPrefix byte = 0x00
	ControlPing   byte = 0x01
	ControlPong   byte = 0x02
)

// MaxPacketSize is the largest UDP payload we send or expect.
const MaxPacketSize = 1500

// RelayTokenLen is the length of the token proving a peer may use the relay.
const RelayTokenLen = 16

// PeerID identifies a connected client, unique per coordinator.
type PeerID uint32

// PacketType is the one-byte tag every data-plane packet starts with.
type PacketType byte

const (
	TypeHandshakeInit   PacketType = 0x01
	TypeHandshakeResp   PacketType = 0x02
	TypeData            PacketType = 0x03
	TypePunch           PacketType = 0x04
	TypeRelayRegister   PacketType = 0x10
	TypeRelayForward    PacketType = 0x11
	TypeRelayDeliver    PacketType = 0x12
	TypeReflectRequest  PacketType = 0x20
	TypeReflectResponse PacketType = 0x21
)

var (
	// ErrShort means the buffer ended before the packet did.
	ErrShort = errors.New("packet too short")
	// ErrUnknownType means the leading tag is not one we speak.
	ErrUnknownType = errors.New("unknown packet type")
)

// Packet is a decoded data-plane packet. Payload slices alias the buffer they
// were decoded from, so a relayed packet can be forwarded without copying.
type Packet struct {
	Type PacketType

	// Handshake and data.
	SenderIndex   uint32
	ReceiverIndex uint32
	Counter       uint64
	Noise         []byte
	Ciphertext    []byte

	// Relay.
	PeerID PeerID
	Token  [RelayTokenLen]byte
	Dst    PeerID
	Src    PeerID
	Inner  []byte

	// Reflection.
	TxID     uint64
	Observed netip.AddrPort

	// Punch.
	Sender PeerID
}

// Decode parses one packet. See the Rust doc comment for the layouts.
func Decode(buf []byte) (Packet, error) {
	var p Packet
	if len(buf) < 1 {
		return p, ErrShort
	}
	p.Type = PacketType(buf[0])

	need := func(n int) error {
		if len(buf) < n {
			return fmt.Errorf("%w: have %d, need %d", ErrShort, len(buf), n)
		}
		return nil
	}

	switch p.Type {
	case TypeHandshakeInit:
		if err := need(5); err != nil {
			return p, err
		}
		p.SenderIndex = binary.BigEndian.Uint32(buf[1:5])
		p.Noise = buf[5:]

	case TypeHandshakeResp:
		if err := need(9); err != nil {
			return p, err
		}
		p.SenderIndex = binary.BigEndian.Uint32(buf[1:5])
		p.ReceiverIndex = binary.BigEndian.Uint32(buf[5:9])
		p.Noise = buf[9:]

	case TypeData:
		if err := need(13); err != nil {
			return p, err
		}
		p.ReceiverIndex = binary.BigEndian.Uint32(buf[1:5])
		p.Counter = binary.BigEndian.Uint64(buf[5:13])
		p.Ciphertext = buf[13:]

	case TypePunch:
		if err := need(5); err != nil {
			return p, err
		}
		p.Sender = PeerID(binary.BigEndian.Uint32(buf[1:5]))

	case TypeRelayRegister:
		if err := need(5 + RelayTokenLen); err != nil {
			return p, err
		}
		p.PeerID = PeerID(binary.BigEndian.Uint32(buf[1:5]))
		copy(p.Token[:], buf[5:5+RelayTokenLen])

	case TypeRelayForward:
		if err := need(5); err != nil {
			return p, err
		}
		p.Dst = PeerID(binary.BigEndian.Uint32(buf[1:5]))
		p.Inner = buf[5:]

	case TypeRelayDeliver:
		if err := need(5); err != nil {
			return p, err
		}
		p.Src = PeerID(binary.BigEndian.Uint32(buf[1:5]))
		p.Inner = buf[5:]

	case TypeReflectRequest:
		if err := need(9); err != nil {
			return p, err
		}
		p.TxID = binary.BigEndian.Uint64(buf[1:9])

	case TypeReflectResponse:
		if err := need(9); err != nil {
			return p, err
		}
		p.TxID = binary.BigEndian.Uint64(buf[1:9])
		addr, err := DecodeAddr(buf[9:])
		if err != nil {
			return p, err
		}
		p.Observed = addr

	default:
		return p, fmt.Errorf("%w: 0x%02x", ErrUnknownType, buf[0])
	}
	return p, nil
}

// Encode serialises a packet, appending to dst and returning the result.
func (p Packet) Encode(dst []byte) []byte {
	dst = append(dst[:0], byte(p.Type))
	switch p.Type {
	case TypeHandshakeInit:
		dst = binary.BigEndian.AppendUint32(dst, p.SenderIndex)
		dst = append(dst, p.Noise...)
	case TypeHandshakeResp:
		dst = binary.BigEndian.AppendUint32(dst, p.SenderIndex)
		dst = binary.BigEndian.AppendUint32(dst, p.ReceiverIndex)
		dst = append(dst, p.Noise...)
	case TypeData:
		dst = binary.BigEndian.AppendUint32(dst, p.ReceiverIndex)
		dst = binary.BigEndian.AppendUint64(dst, p.Counter)
		dst = append(dst, p.Ciphertext...)
	case TypePunch:
		dst = binary.BigEndian.AppendUint32(dst, uint32(p.Sender))
	case TypeRelayRegister:
		dst = binary.BigEndian.AppendUint32(dst, uint32(p.PeerID))
		dst = append(dst, p.Token[:]...)
	case TypeRelayForward:
		dst = binary.BigEndian.AppendUint32(dst, uint32(p.Dst))
		dst = append(dst, p.Inner...)
	case TypeRelayDeliver:
		dst = binary.BigEndian.AppendUint32(dst, uint32(p.Src))
		dst = append(dst, p.Inner...)
	case TypeReflectRequest:
		dst = binary.BigEndian.AppendUint64(dst, p.TxID)
	case TypeReflectResponse:
		dst = binary.BigEndian.AppendUint64(dst, p.TxID)
		dst = EncodeAddr(dst, p.Observed)
	}
	return dst
}

// EncodeAddr writes `[family: 4|6][addr bytes][port: u16]`.
func EncodeAddr(dst []byte, addr netip.AddrPort) []byte {
	ip := addr.Addr()
	if ip.Is4() {
		v4 := ip.As4()
		dst = append(dst, 4)
		dst = append(dst, v4[:]...)
	} else {
		v6 := ip.As16()
		dst = append(dst, 6)
		dst = append(dst, v6[:]...)
	}
	return binary.BigEndian.AppendUint16(dst, addr.Port())
}

// DecodeAddr reads an address written by EncodeAddr.
func DecodeAddr(buf []byte) (netip.AddrPort, error) {
	if len(buf) < 1 {
		return netip.AddrPort{}, ErrShort
	}
	switch buf[0] {
	case 4:
		if len(buf) < 7 {
			return netip.AddrPort{}, ErrShort
		}
		return netip.AddrPortFrom(
			netip.AddrFrom4([4]byte{buf[1], buf[2], buf[3], buf[4]}),
			binary.BigEndian.Uint16(buf[5:7]),
		), nil
	case 6:
		if len(buf) < 19 {
			return netip.AddrPort{}, ErrShort
		}
		var octets [16]byte
		copy(octets[:], buf[1:17])
		return netip.AddrPortFrom(
			netip.AddrFrom16(octets),
			binary.BigEndian.Uint16(buf[17:19]),
		), nil
	default:
		return netip.AddrPort{}, fmt.Errorf("%w: address family 0x%02x", ErrUnknownType, buf[0])
	}
}

// ToAddrPort converts a net.Addr from a UDP socket, normalising the
// IPv4-in-IPv6 form the Go runtime hands back so that addresses compare equal.
func ToAddrPort(addr net.Addr) (netip.AddrPort, bool) {
	udp, ok := addr.(*net.UDPAddr)
	if !ok {
		return netip.AddrPort{}, false
	}
	ip, ok := netip.AddrFromSlice(udp.IP)
	if !ok {
		return netip.AddrPort{}, false
	}
	return netip.AddrPortFrom(ip.Unmap(), uint16(udp.Port)), true
}

// ReplayWindow rejects repeated and stale packet counters, the scheme WireGuard
// uses: a high-water mark plus a bitmap of the packets seen just below it.
type ReplayWindow struct {
	highest uint64
	bitmap  uint64
}

// WindowSize is how far behind the highest counter a packet may still arrive.
const WindowSize = 64

// Accept records a counter and reports whether it was fresh.
func (w *ReplayWindow) Accept(counter uint64) bool {
	if counter > w.highest {
		shift := counter - w.highest
		if shift >= WindowSize {
			w.bitmap = 0
		} else {
			w.bitmap <<= shift
		}
		w.bitmap |= 1
		w.highest = counter
		return true
	}
	behind := w.highest - counter
	if behind >= WindowSize {
		return false
	}
	mask := uint64(1) << behind
	if w.bitmap&mask != 0 {
		return false
	}
	w.bitmap |= mask
	return true
}
