package client

import (
	"encoding/json"
	"net/netip"

	"github.com/finhex/hamradnet/internal/proto"
)

// minIPv4Header is the smallest possible IPv4 header.
const minIPv4Header = 20

func isIPv4(packet []byte) bool {
	return len(packet) >= minIPv4Header && packet[0]>>4 == 4
}

// ipv4Destination reads the destination address out of an IPv4 packet.
func ipv4Destination(packet []byte) (netip.Addr, bool) {
	if !isIPv4(packet) {
		return netip.Addr{}, false
	}
	return netip.AddrFrom4([4]byte{packet[16], packet[17], packet[18], packet[19]}), true
}

// ipv4Source reads the source address out of an IPv4 packet.
func ipv4Source(packet []byte) (netip.Addr, bool) {
	if !isIPv4(packet) {
		return netip.Addr{}, false
	}
	return netip.AddrFrom4([4]byte{packet[12], packet[13], packet[14], packet[15]}), true
}

// isFanout reports whether a destination should reach every peer rather than one.
//
// This is the piece that makes "scan for LAN games" work. A game looking for
// servers sends its probe to the broadcast address; on a real LAN the switch
// floods it to every port, and here it is copied to every peer. Multicast is
// included because newer titles, and mDNS or UPnP discovery, use that instead.
func isFanout(addr netip.Addr) bool {
	if !addr.Is4() {
		return false
	}
	if addr == netip.AddrFrom4([4]byte{255, 255, 255, 255}) {
		return true
	}
	if addr == proto.VirtualBroadcast {
		return true
	}
	return addr.IsMulticast()
}

// serverMsg is a decoded server -> client frame. Only the fields belonging to
// the variant named by T are meaningful.
type serverMsg struct {
	T string `json:"t"`

	Version    uint32           `json:"version"`
	PeerID     proto.PeerID     `json:"peer_id"`
	VirtualIP  netip.Addr       `json:"virtual_ip"`
	PrefixLen  uint8            `json:"prefix_len"`
	UDPPort    uint16           `json:"udp_port"`
	RelayToken proto.RelayToken `json:"relay_token"`

	NetworkID string            `json:"network_id"`
	Name      string            `json:"name"`
	Network   proto.NetworkInfo `json:"network"`
	Peers     []proto.PeerInfo  `json:"peers"`
	Peer      proto.PeerInfo    `json:"peer"`

	Code    string `json:"code"`
	Message string `json:"message"`
}

// parseServerMsg decodes one frame. The Rust side writes these as serde
// internally-tagged enums, so the variant's fields sit flat beside "t" and a
// single struct with everything optional decodes any of them.
func parseServerMsg(frame []byte) (serverMsg, error) {
	var msg serverMsg
	err := json.Unmarshal(frame, &msg)
	return msg, err
}
