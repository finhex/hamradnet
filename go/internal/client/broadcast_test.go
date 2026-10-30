package client

import (
	"bytes"
	"encoding/binary"
	"net/netip"
	"testing"
)

// udpPacket builds an IPv4 UDP packet with correct checksums.
func udpPacket(src, dst [4]byte, srcPort, dstPort uint16, payload []byte) []byte {
	total := 20 + 8 + len(payload)
	p := make([]byte, total)
	p[0] = 0x45
	binary.BigEndian.PutUint16(p[2:4], uint16(total))
	p[8], p[9] = 64, 17
	copy(p[12:16], src[:])
	copy(p[16:20], dst[:])
	binary.BigEndian.PutUint16(p[10:12], checksum(p[:20], 0))
	binary.BigEndian.PutUint16(p[20:22], srcPort)
	binary.BigEndian.PutUint16(p[22:24], dstPort)
	binary.BigEndian.PutUint16(p[24:26], uint16(8+len(payload)))
	copy(p[28:], payload)
	binary.BigEndian.PutUint16(p[26:28], checksum(p[20:], udpPseudoSum(p)))
	return p
}

func udpPseudoSum(p []byte) uint32 {
	sum := uint32(17) + uint32(len(p)-20)
	for i := 12; i < 20; i += 2 {
		sum += uint32(binary.BigEndian.Uint16(p[i : i+2]))
	}
	return sum
}

var (
	lanAddr    = [4]byte{192, 168, 1, 5}
	tunnelAddr = netip.MustParseAddr("10.66.0.2")
)

func TestRebroadcastSendsFromTheTunnelAddressWithValidChecksums(t *testing.T) {
	payload := []byte("\xf7\x2f\x10\x00PX3W\x1c\x00\x00\x00\x00\x00\x00\x00") // a WC3 search
	original := udpPacket(lanAddr, limitedBroadcast, 6112, 6112, payload)

	out, ok := rebroadcast(original, tunnelAddr)
	if !ok {
		t.Fatal("a game's broadcast was not copied")
	}
	if got := netip.AddrFrom4([4]byte(out[12:16])); got != tunnelAddr {
		t.Errorf("source is %s, want %s", got, tunnelAddr)
	}
	if checksum(out[:20], 0) != 0 {
		t.Error("the IP header checksum does not verify")
	}
	if checksum(out[20:], udpPseudoSum(out)) != 0 {
		t.Error("the UDP checksum does not verify")
	}
	if !bytes.Equal(out[28:], payload) || !bytes.Equal(out[16:20], limitedBroadcast[:]) {
		t.Error("the destination or payload changed")
	}
	if !bytes.Equal(original[12:16], lanAddr[:]) {
		t.Error("the original packet was modified")
	}
}

// A packet caught on its way out may carry only the partial sum the network
// card was meant to complete; the copy must still be valid.
func TestRebroadcastRepairsAnUnfinishedChecksum(t *testing.T) {
	p := udpPacket(lanAddr, limitedBroadcast, 6112, 6112, []byte("hello"))
	binary.BigEndian.PutUint16(p[26:28], 0x1234)
	out, ok := rebroadcast(p, tunnelAddr)
	if !ok || checksum(out[20:], udpPseudoSum(out)) != 0 {
		t.Fatal("the copy's UDP checksum does not verify")
	}
}

func TestRebroadcastLeavesEverythingElseAlone(t *testing.T) {
	game := udpPacket(lanAddr, limitedBroadcast, 6112, 6112, []byte("x"))

	fragment := append([]byte(nil), game...)
	fragment[6] = 0x20 // more fragments follow

	tcp := append([]byte(nil), game...)
	tcp[9] = 6

	truncated := game[:len(game)-1]

	for name, packet := range map[string][]byte{
		"unicast":       udpPacket(lanAddr, [4]byte{192, 168, 1, 9}, 6112, 6112, []byte("x")),
		"subnet":        udpPacket(lanAddr, [4]byte{192, 168, 1, 255}, 6112, 6112, []byte("x")),
		"KDE Connect":   udpPacket(lanAddr, limitedBroadcast, 1716, 1716, []byte("x")),
		"Syncthing":     udpPacket(lanAddr, limitedBroadcast, 21027, 21027, []byte("x")),
		"DHCP":          udpPacket([4]byte{}, limitedBroadcast, 68, 67, []byte("x")),
		"too big":       udpPacket(lanAddr, limitedBroadcast, 6112, 6112, make([]byte, 1300)),
		"fragment":      fragment,
		"TCP":           tcp,
		"truncated":     truncated,
		"not even IPv4": {0x60, 0, 0, 0},
	} {
		if _, ok := rebroadcast(packet, tunnelAddr); ok {
			t.Errorf("%s was copied into the tunnel", name)
		}
	}
}
