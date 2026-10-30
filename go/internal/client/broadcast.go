package client

import (
	"encoding/binary"
	"net/netip"

	"github.com/finhex/hamradnet/internal/proto"
)

// A broadcast to 255.255.255.255 leaves the computer through one interface
// only, the one the default route uses: the real network card, never the
// tunnel. A game that announces itself that way, as Warcraft III and many
// others do, would therefore never be heard by the other players.
//
// So the daemon watches for such broadcasts going out of every other interface
// and sends a copy through the tunnel too, from this machine's tunnel address
// so that answers come back the same way. The original still reaches the local
// network. On Linux that is captureBroadcasts; on Windows, which offers no such
// view, the tunnel is made the interface broadcasts prefer instead.

// skippedBroadcastPorts belong to services that announce this computer to its
// own network, not to games, so they stay off the tunnel: DHCP, NetBIOS, KDE
// Connect, Syncthing and Steam's streaming discovery. The Rust client skips
// the same list.
var skippedBroadcastPorts = map[uint16]bool{
	67: true, 68: true, 137: true, 138: true, 1716: true, 21027: true, 27036: true,
}

var limitedBroadcast = [4]byte{255, 255, 255, 255}

// rebroadcast turns a broadcast seen leaving another interface into one for the
// tunnel: the same packet, sent from the given tunnel address. It reports false
// for anything that is not a whole UDP broadcast worth copying.
func rebroadcast(packet []byte, source netip.Addr) ([]byte, bool) {
	if len(packet) < 20 || packet[0]>>4 != 4 || !source.Is4() {
		return nil, false
	}
	headerLen := int(packet[0]&0x0f) * 4
	total := int(binary.BigEndian.Uint16(packet[2:4]))
	if headerLen < 20 || total < headerLen+8 || total > len(packet) || total > proto.TunMTU {
		return nil, false
	}
	// Only the first fragment has a UDP header, so fragments are left alone.
	if binary.BigEndian.Uint16(packet[6:8])&0x3fff != 0 || packet[9] != 17 ||
		[4]byte(packet[16:20]) != limitedBroadcast {
		return nil, false
	}
	udp := packet[headerLen:total]
	if int(binary.BigEndian.Uint16(udp[4:6])) != len(udp) ||
		skippedBroadcastPorts[binary.BigEndian.Uint16(udp[2:4])] {
		return nil, false
	}

	out := append([]byte(nil), packet[:total]...)
	src := source.As4()
	copy(out[12:16], src[:])
	out[10], out[11] = 0, 0
	binary.BigEndian.PutUint16(out[10:12], checksum(out[:headerLen], 0))

	// The UDP checksum covers the addresses too. It is computed afresh rather
	// than adjusted: a packet caught on its way out may still hold the partial
	// sum the network card was going to finish.
	segment := out[headerLen:]
	segment[6], segment[7] = 0, 0
	pseudo := uint32(17) + uint32(len(segment))
	for i := 12; i < 20; i += 2 {
		pseudo += uint32(binary.BigEndian.Uint16(out[i : i+2]))
	}
	sum := checksum(segment, pseudo)
	if sum == 0 {
		// Zero means "no checksum" in UDP, so a real zero is sent as all ones.
		sum = 0xffff
	}
	binary.BigEndian.PutUint16(segment[6:8], sum)
	return out, true
}

// checksum is the Internet checksum of data, folded in on top of initial.
func checksum(data []byte, initial uint32) uint16 {
	sum := initial
	for i := 0; i+1 < len(data); i += 2 {
		sum += uint32(binary.BigEndian.Uint16(data[i : i+2]))
	}
	if len(data)%2 == 1 {
		sum += uint32(data[len(data)-1]) << 8
	}
	for sum>>16 != 0 {
		sum = sum&0xffff + sum>>16
	}
	return ^uint16(sum)
}
