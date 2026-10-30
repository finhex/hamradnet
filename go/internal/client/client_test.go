package client

import (
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/finhex/hamradnet/internal/proto"
)

// packet builds a minimal well-formed IPv4 header.
func packet(src, dst [4]byte) []byte {
	buf := make([]byte, minIPv4Header)
	buf[0] = 0x45 // version 4, five 32-bit words of header
	buf[9] = 17   // UDP
	copy(buf[12:16], src[:])
	copy(buf[16:20], dst[:])
	return buf
}

func TestReadsSourceAndDestination(t *testing.T) {
	p := packet([4]byte{10, 66, 0, 2}, [4]byte{10, 66, 0, 7})
	src, ok := ipv4Source(p)
	if !ok || src != netip.MustParseAddr("10.66.0.2") {
		t.Errorf("source: got %v", src)
	}
	dst, ok := ipv4Destination(p)
	if !ok || dst != netip.MustParseAddr("10.66.0.7") {
		t.Errorf("destination: got %v", dst)
	}
}

func TestRejectsTruncatedAndNonIPv4(t *testing.T) {
	if _, ok := ipv4Destination(nil); ok {
		t.Error("an empty buffer is not a packet")
	}
	if _, ok := ipv4Destination(make([]byte, minIPv4Header-1)); ok {
		t.Error("a truncated header must be rejected")
	}
	ipv6 := make([]byte, 40)
	ipv6[0] = 0x60
	if _, ok := ipv4Destination(ipv6); ok {
		t.Error("IPv6 is not routed")
	}
}

func TestRecognisesAddressesThatMustReachEveryone(t *testing.T) {
	for _, text := range []string{
		"255.255.255.255", // broadcast
		"10.66.255.255",   // the tunnel's own broadcast
		"224.0.0.251",     // mDNS
		"239.255.255.250", // SSDP/UPnP
		"224.0.252.28",    // Minecraft LAN discovery
	} {
		if !isFanout(netip.MustParseAddr(text)) {
			t.Errorf("%s should reach every peer", text)
		}
	}
}

func TestOrdinaryPeerAddressesAreNotFanout(t *testing.T) {
	for _, text := range []string{"10.66.0.7", "10.66.1.1"} {
		if isFanout(netip.MustParseAddr(text)) {
			t.Errorf("%s should go to one peer", text)
		}
	}
}

func info(id proto.PeerID, lastOctet byte) proto.PeerInfo {
	var key proto.PublicKey
	key[0] = byte(id)
	return proto.PeerInfo{
		PeerID:    id,
		Name:      "peer",
		PublicKey: key,
		VirtualIP: netip.AddrFrom4([4]byte{10, 66, 0, lastOctet}),
		Endpoints: []netip.AddrPort{
			netip.AddrPortFrom(netip.AddrFrom4([4]byte{203, 0, 113, lastOctet}), 7000),
		},
	}
}

func TestRoutesByVirtualIPAndKey(t *testing.T) {
	table := NewPeerTable()
	table.Upsert(info(7, 7))

	if id, ok := table.ByVirtualIP(netip.MustParseAddr("10.66.0.7")); !ok || id != 7 {
		t.Errorf("by address: got %d %v", id, ok)
	}
	var key proto.PublicKey
	key[0] = 7
	if id, ok := table.ByPublicKey(key); !ok || id != 7 {
		t.Errorf("by key: got %d %v", id, ok)
	}
	if _, ok := table.ByVirtualIP(netip.MustParseAddr("10.66.0.9")); ok {
		t.Error("an unknown address should not resolve")
	}
}

func TestRemovingAPeerClearsEveryIndex(t *testing.T) {
	table := NewPeerTable()
	table.Upsert(info(7, 7))
	table.BindIndex(7, 4242)

	table.Remove(7)
	if table.Len() != 0 {
		t.Error("the peer should be gone")
	}
	if _, ok := table.ByVirtualIP(netip.MustParseAddr("10.66.0.7")); ok {
		t.Error("the address index should be cleared")
	}
	if _, ok := table.BySessionIndex(4242); ok {
		t.Error("the session index should be cleared")
	}
}

func TestRebindingAnIndexDropsThePreviousOne(t *testing.T) {
	table := NewPeerTable()
	table.Upsert(info(7, 7))
	table.BindIndex(7, 1)
	table.BindIndex(7, 2)
	if _, ok := table.BySessionIndex(1); ok {
		t.Error("the old index should be gone")
	}
	if id, ok := table.BySessionIndex(2); !ok || id != 7 {
		t.Error("the new index should resolve")
	}
}

func TestRosterUpdatesKeepAConfirmedPath(t *testing.T) {
	table := NewPeerTable()
	peer := table.Upsert(info(7, 7))
	confirmed := netip.MustParseAddrPort("198.51.100.4:9999")
	peer.Confirmed, peer.HasConfirmed = confirmed, true

	updated := info(7, 7)
	updated.Endpoints = []netip.AddrPort{netip.MustParseAddrPort("192.168.1.9:7000")}
	table.Upsert(updated)

	peer, _ = table.Get(7)
	if !peer.HasConfirmed || peer.Confirmed != confirmed {
		t.Error("a confirmed path must survive a roster update")
	}
	if len(peer.Candidates) != 2 {
		t.Errorf("the new candidate should be merged in, got %v", peer.Candidates)
	}
}

func TestChangedVirtualIPLeavesNoStaleRoute(t *testing.T) {
	table := NewPeerTable()
	table.Upsert(info(7, 7))
	table.Upsert(info(7, 8))

	if _, ok := table.ByVirtualIP(netip.MustParseAddr("10.66.0.7")); ok {
		t.Error("the old address should no longer resolve")
	}
	if id, ok := table.ByVirtualIP(netip.MustParseAddr("10.66.0.8")); !ok || id != 7 {
		t.Error("the new address should resolve")
	}
	if table.Len() != 1 {
		t.Errorf("there should still be one peer, got %d", table.Len())
	}
}

func TestPeerInTwoNetworksSurvivesLeavingOne(t *testing.T) {
	table := NewPeerTable()
	table.Upsert(info(7, 7))
	table.AddToNetwork(7, "HAM-A")
	table.AddToNetwork(7, "HAM-B")

	if table.RemoveFromNetwork(7, "HAM-A") {
		t.Error("the peer still shares HAM-B")
	}
	if _, ok := table.Get(7); !ok {
		t.Fatal("the peer must not be dropped")
	}
	if !table.RemoveFromNetwork(7, "HAM-B") {
		t.Error("that was the last network in common")
	}
	if _, ok := table.Get(7); ok {
		t.Error("now the peer should be gone")
	}
}

func TestForgettingANetworkDropsOnlyItsExclusivePeers(t *testing.T) {
	table := NewPeerTable()
	table.Upsert(info(7, 7))
	table.Upsert(info(8, 8))
	table.AddToNetwork(7, "HAM-A")
	table.AddToNetwork(8, "HAM-A")
	table.AddToNetwork(8, "HAM-B")

	table.ForgetNetwork("HAM-A")
	if _, ok := table.Get(7); ok {
		t.Error("a peer only in the network we left should go")
	}
	if _, ok := table.Get(8); !ok {
		t.Error("a peer still sharing HAM-B should stay")
	}
}

func TestOnlyOneSideDials(t *testing.T) {
	if !ShouldInitiate(1, 2) || ShouldInitiate(2, 1) {
		t.Error("the lower ID should dial")
	}
}

func TestPathNameReflectsTheRoute(t *testing.T) {
	table := NewPeerTable()
	peer := table.Upsert(info(7, 7))
	if peer.PathName() != "-" {
		t.Errorf("an idle peer has no path, got %q", peer.PathName())
	}
	peer.Link = LinkUp
	if peer.PathName() != "relay" {
		t.Errorf("unconfirmed means relayed, got %q", peer.PathName())
	}
	peer.Confirmed, peer.HasConfirmed = netip.MustParseAddrPort("203.0.113.7:7000"), true
	if peer.PathName() != "direct" {
		t.Errorf("confirmed means direct, got %q", peer.PathName())
	}
}

func TestSilenceEventuallyMarksAPeerStale(t *testing.T) {
	table := NewPeerTable()
	peer := table.Upsert(info(7, 7))
	now := time.Now()

	if peer.IsStale(now) {
		t.Error("no handshake has been attempted yet")
	}
	peer.LastRX = now
	if peer.IsStale(now) {
		t.Error("just heard from")
	}
	peer.LastRX = now.Add(-PeerTimeout - time.Second)
	if !peer.IsStale(now) {
		t.Error("silence past the timeout is stale")
	}
}

func TestRTTIsSmoothedNotReplaced(t *testing.T) {
	peer := &Peer{}
	peer.RecordRTT(100 * time.Millisecond)
	if peer.RTT != 100*time.Millisecond {
		t.Fatalf("the first sample is taken as-is, got %v", peer.RTT)
	}
	// One slow sample must not dominate the reading.
	peer.RecordRTT(500 * time.Millisecond)
	if peer.RTT <= 100*time.Millisecond || peer.RTT >= 300*time.Millisecond {
		t.Errorf("a single outlier moved the average too far: %v", peer.RTT)
	}
}

func TestControlFrameLayout(t *testing.T) {
	frame := controlFrame(proto.ControlPing, 0x0102030405060708)
	if len(frame) != 10 {
		t.Fatalf("length: got %d", len(frame))
	}
	// No IP packet can begin with a zero byte, which is what makes these safe to
	// mix with real traffic.
	if frame[0] != proto.ControlPrefix || frame[1] != proto.ControlPing {
		t.Errorf("header: got %x", frame[:2])
	}
}

func TestConfigRoundTripsThroughDisk(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config.json")

	config, err := GenerateConfig("vpn.example.com:7777", "konako")
	if err != nil {
		t.Fatal(err)
	}
	config.RememberNetwork("HAM-1111-2222", "pw")
	if err := config.Save(path); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadConfig(path)
	if err != nil {
		t.Fatal(err)
	}
	if loaded.Server != "vpn.example.com:7777" || loaded.Name != "konako" {
		t.Errorf("got %+v", loaded)
	}
	if len(loaded.Networks) != 1 || loaded.Networks[0].NetworkID != "HAM-1111-2222" {
		t.Errorf("networks: got %+v", loaded.Networks)
	}
	if loaded.PrivateKey != config.PrivateKey {
		t.Error("the identity key must survive")
	}

	// It holds a private key and passwords, so it must not be world-readable.
	stat, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if perm := stat.Mode().Perm(); perm != 0o600 {
		t.Errorf("permissions: got %o, want 600", perm)
	}
}

func TestServerAddressesGetTheDefaultPort(t *testing.T) {
	for typed, want := range map[string]string{
		"203.0.113.45":       "203.0.113.45:7777",
		" 203.0.113.45 ":     "203.0.113.45:7777",
		"203.0.113.45:9000":  "203.0.113.45:9000",
		"vpn.example.com":    "vpn.example.com:7777",
		"2001:db8::1":        "[2001:db8::1]:7777",
		"[2001:db8::1]:7777": "[2001:db8::1]:7777",
		"":                   "",
	} {
		if got := NormalizeServer(typed); got != want {
			t.Errorf("NormalizeServer(%q) = %q, want %q", typed, got, want)
		}
	}
}

// The Rust client shares the config file and cannot load "networks": null, so a
// fresh config, never joined to anything, must still write a list.
func TestAFreshConfigWritesAnEmptyListForRust(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.json")
	config, err := GenerateConfig("s:1", "n")
	if err != nil {
		t.Fatal(err)
	}
	if err := config.Save(path); err != nil {
		t.Fatal(err)
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(raw), `"networks": []`) {
		t.Errorf("wrote %s", raw)
	}
}

func TestRememberingTheSameNetworkTwiceUpdatesRatherThanDuplicates(t *testing.T) {
	config, err := GenerateConfig("s:1", "n")
	if err != nil {
		t.Fatal(err)
	}
	config.RememberNetwork("HAM-1", "old")
	config.RememberNetwork("HAM-1", "new")
	if len(config.Networks) != 1 || config.Networks[0].Password != "new" {
		t.Errorf("got %+v", config.Networks)
	}
	config.ForgetNetwork("HAM-1")
	if len(config.Networks) != 0 {
		t.Errorf("should be empty, got %+v", config.Networks)
	}
}

func TestGeneratedKeysAreDistinctAndDerivable(t *testing.T) {
	a, err := GenerateConfig("s:1", "n")
	if err != nil {
		t.Fatal(err)
	}
	b, err := GenerateConfig("s:1", "n")
	if err != nil {
		t.Fatal(err)
	}
	if a.PrivateKey == b.PrivateKey {
		t.Error("two identities must differ")
	}
	if _, err := a.PublicKey(); err != nil {
		t.Errorf("the public half should derive: %v", err)
	}
}

func TestNetmaskFromPrefix(t *testing.T) {
	cases := map[uint8]string{
		0: "0.0.0.0", 8: "255.0.0.0", 16: "255.255.0.0",
		24: "255.255.255.0", 32: "255.255.255.255",
	}
	for prefix, want := range cases {
		if got := netmask(prefix); got != want {
			t.Errorf("/%d: got %s, want %s", prefix, got, want)
		}
	}
}
