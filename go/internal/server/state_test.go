package server

import (
	"encoding/hex"
	"io"
	"log/slog"
	"net/netip"
	"testing"
	"time"

	"github.com/finhex/hamradnet/internal/proto"
)

func quietState(grace time.Duration) *State {
	return NewState("", grace, slog.New(slog.NewTextHandler(io.Discard, nil)))
}

func testPeer(t *testing.T, s *State, name string, key proto.PublicKey) proto.PeerID {
	t.Helper()
	peer, ok := s.RegisterPeer(name, key)
	if !ok {
		t.Fatalf("could not register %s", name)
	}
	return peer.ID
}

func TestAllocatorSkipsReservedAddressesAndReusesReleased(t *testing.T) {
	a := newIPAllocator()
	first, _ := a.allocate()
	if first != netip.MustParseAddr("10.66.0.2") {
		t.Fatalf("first address: got %v", first)
	}
	second, _ := a.allocate()
	if second != netip.MustParseAddr("10.66.0.3") {
		t.Fatalf("second address: got %v", second)
	}
	a.release(second)
	again, _ := a.allocate()
	if again != second {
		t.Errorf("a released address should be reused, got %v", again)
	}
}

func TestAllocatorCrossesOctetBoundary(t *testing.T) {
	a := newIPAllocator()
	var last netip.Addr
	for i := 0; i < 300; i++ {
		last, _ = a.allocate()
	}
	if last != netip.MustParseAddr("10.66.1.45") {
		t.Errorf("got %v", last)
	}
}

func TestWrongPasswordIsRejected(t *testing.T) {
	s := quietState(0)
	peer := testPeer(t, s, "a", proto.PublicKey{1})
	id := s.CreateNetwork("games", "hunter2", proto.PublicKey{1})

	if _, _, code := s.JoinNetwork(peer, id, "wrong"); code != proto.ErrBadPassword {
		t.Errorf("expected bad_password, got %q", code)
	}
	if _, _, code := s.JoinNetwork(peer, id, "hunter2"); code != "" {
		t.Errorf("the right password should work, got %q", code)
	}
}

func TestUnknownNetworkIsRejected(t *testing.T) {
	s := quietState(0)
	peer := testPeer(t, s, "a", proto.PublicKey{1})
	if _, _, code := s.JoinNetwork(peer, "HAM-0000-0000", "x"); code != proto.ErrNoSuchNetwork {
		t.Errorf("got %q", code)
	}
}

func TestJoiningTwiceIsRejected(t *testing.T) {
	s := quietState(0)
	peer := testPeer(t, s, "a", proto.PublicKey{1})
	id := s.CreateNetwork("games", "pw", proto.PublicKey{1})
	if _, _, code := s.JoinNetwork(peer, id, "pw"); code != "" {
		t.Fatalf("first join failed: %q", code)
	}
	if _, _, code := s.JoinNetwork(peer, id, "pw"); code != proto.ErrAlreadyJoined {
		t.Errorf("got %q", code)
	}
}

func TestSecondPeerSeesTheFirstInItsRoster(t *testing.T) {
	s := quietState(0)
	first := testPeer(t, s, "first", proto.PublicKey{1})
	second := testPeer(t, s, "second", proto.PublicKey{2})
	id := s.CreateNetwork("games", "pw", proto.PublicKey{1})

	_, roster, _ := s.JoinNetwork(first, id, "pw")
	if len(roster) != 0 {
		t.Errorf("the first peer should see an empty network, got %d", len(roster))
	}

	info, roster, _ := s.JoinNetwork(second, id, "pw")
	if len(roster) != 1 || roster[0].PeerID != first {
		t.Fatalf("roster: got %+v", roster)
	}
	if info.VirtualIP == roster[0].VirtualIP {
		t.Error("addresses must be unique")
	}
}

func TestTheCreatorCanDeleteAndOthersCannot(t *testing.T) {
	s := quietState(0)
	ownerKey := proto.PublicKey{9}
	owner := testPeer(t, s, "owner", ownerKey)
	guest := testPeer(t, s, "guest", proto.PublicKey{3})

	id := s.CreateNetwork("games", "pw", ownerKey)
	s.JoinNetwork(owner, id, "pw")
	s.JoinNetwork(guest, id, "pw")

	// A member who did not create it is refused, even knowing the password.
	if code := s.DeleteNetwork(guest, id); code != proto.ErrNotOwner {
		t.Errorf("a guest should be refused, got %q", code)
	}
	if !s.HasNetwork(id) {
		t.Fatal("the network should still exist")
	}
	if code := s.DeleteNetwork(owner, id); code != "" {
		t.Errorf("the owner should succeed, got %q", code)
	}
	if s.HasNetwork(id) {
		t.Error("the network should be gone")
	}
}

// Networks with no owner recorded predate ownership; a member may remove them,
// but a stranger may not.
func TestLegacyNetworkCanBeDeletedByAMemberOnly(t *testing.T) {
	s := quietState(0)
	member := testPeer(t, s, "member", proto.PublicKey{1})
	stranger := testPeer(t, s, "stranger", proto.PublicKey{4})

	id := s.CreateNetwork("old", "pw", proto.PublicKey{})
	// Clear the owner the way a store written before ownership existed would.
	s.mu.Lock()
	s.Networks[id].Owner = ""
	s.mu.Unlock()
	s.JoinNetwork(member, id, "pw")

	if code := s.DeleteNetwork(stranger, id); code != proto.ErrNotOwner {
		t.Errorf("a stranger should be refused, got %q", code)
	}
	if code := s.DeleteNetwork(member, id); code != "" {
		t.Errorf("a member should succeed, got %q", code)
	}
}

func TestOccupiedNetworkIsNeverRemoved(t *testing.T) {
	s := quietState(0)
	peer := testPeer(t, s, "a", proto.PublicKey{1})
	id := s.CreateNetwork("games", "pw", proto.PublicKey{1})
	s.JoinNetwork(peer, id, "pw")

	if removed := s.SweepEmpty(); removed != 0 {
		t.Errorf("nothing should be swept, got %d", removed)
	}
	if !s.HasNetwork(id) {
		t.Error("an occupied network must stay")
	}
}

func TestNetworkGoesWhenTheLastMemberLeaves(t *testing.T) {
	s := quietState(0)
	first := testPeer(t, s, "first", proto.PublicKey{1})
	second := testPeer(t, s, "second", proto.PublicKey{2})
	id := s.CreateNetwork("games", "pw", proto.PublicKey{1})
	s.JoinNetwork(first, id, "pw")
	s.JoinNetwork(second, id, "pw")

	s.LeaveNetwork(first, id)
	if !s.HasNetwork(id) {
		t.Fatal("someone is still in it")
	}
	s.LeaveNetwork(second, id)
	if s.HasNetwork(id) {
		t.Error("the last one out should close it")
	}
}

func TestDisconnectingCountsAsLeaving(t *testing.T) {
	s := quietState(0)
	peer := testPeer(t, s, "a", proto.PublicKey{1})
	id := s.CreateNetwork("games", "pw", proto.PublicKey{1})
	s.JoinNetwork(peer, id, "pw")

	s.RemovePeer(peer)
	if s.HasNetwork(id) {
		t.Error("dropping off should close an otherwise empty network")
	}
}

// A network is memberless between being created and its creator joining.
// Removing it in that gap would be a race the client could never win.
func TestFreshlyCreatedNetworkSurvivesUntilItsCreatorJoins(t *testing.T) {
	s := quietState(0)
	peer := testPeer(t, s, "a", proto.PublicKey{1})
	id := s.CreateNetwork("games", "pw", proto.PublicKey{1})

	if removed := s.SweepEmpty(); removed != 0 {
		t.Errorf("nobody has ever joined it yet, got %d removed", removed)
	}
	if _, _, code := s.JoinNetwork(peer, id, "pw"); code != "" {
		t.Errorf("the creator should still be able to join, got %q", code)
	}
}

func TestGracePeriodHoldsTheNetworkBriefly(t *testing.T) {
	s := quietState(time.Minute)
	peer := testPeer(t, s, "a", proto.PublicKey{1})
	id := s.CreateNetwork("games", "pw", proto.PublicKey{1})
	s.JoinNetwork(peer, id, "pw")

	s.LeaveNetwork(peer, id)
	if !s.HasNetwork(id) {
		t.Fatal("it should be held during the grace period")
	}
	if removed := s.SweepEmpty(); removed != 0 {
		t.Errorf("too early to sweep, got %d", removed)
	}

	// Once the grace has elapsed it goes.
	s.mu.Lock()
	s.Networks[id].emptySince = time.Now().Add(-2 * time.Minute)
	s.mu.Unlock()
	if removed := s.SweepEmpty(); removed != 1 {
		t.Errorf("expected one removal, got %d", removed)
	}
}

func TestReconnectingWithinTheGraceSavesTheNetwork(t *testing.T) {
	s := quietState(time.Minute)
	peer := testPeer(t, s, "a", proto.PublicKey{1})
	id := s.CreateNetwork("games", "pw", proto.PublicKey{1})
	s.JoinNetwork(peer, id, "pw")
	s.LeaveNetwork(peer, id)

	// Back before the grace runs out, as after a dropped link.
	returning := testPeer(t, s, "a-again", proto.PublicKey{5})
	if _, _, code := s.JoinNetwork(returning, id, "pw"); code != "" {
		t.Fatalf("rejoining failed: %q", code)
	}
	s.mu.Lock()
	pending := s.Networks[id].emptySince
	s.mu.Unlock()
	if !pending.IsZero() {
		t.Error("rejoining should cancel the pending removal")
	}
	if removed := s.SweepEmpty(); removed != 0 {
		t.Errorf("nothing should be swept, got %d", removed)
	}
}

func TestNetworkCreatedAndNeverJoinedIsEventuallyCleanedUp(t *testing.T) {
	s := quietState(0)
	id := s.CreateNetwork("games", "pw", proto.PublicKey{1})
	s.mu.Lock()
	s.Networks[id].LastActive = time.Now().Add(-AbandonedAfter - time.Hour).Unix()
	s.mu.Unlock()
	if removed := s.SweepEmpty(); removed != 1 {
		t.Errorf("expected one removal, got %d", removed)
	}
}

func TestRemovingAPeerFreesItsAddress(t *testing.T) {
	s := quietState(0)
	peer := testPeer(t, s, "a", proto.PublicKey{1})
	s.mu.Lock()
	ip := s.Peers[peer].VirtualIP
	s.mu.Unlock()

	s.RemovePeer(peer)
	next := testPeer(t, s, "b", proto.PublicKey{2})
	s.mu.Lock()
	reused := s.Peers[next].VirtualIP
	s.mu.Unlock()
	if reused != ip {
		t.Errorf("the address should be recycled: got %v, want %v", reused, ip)
	}
}

func TestObservedEndpointIsOfferedBeforeReportedOnes(t *testing.T) {
	s := quietState(0)
	peer := testPeer(t, s, "a", proto.PublicKey{1})
	lan := netip.MustParseAddrPort("192.168.1.5:7000")
	public := netip.MustParseAddrPort("203.0.113.7:41234")

	if !s.UpdateEndpoints(peer, []netip.AddrPort{lan}, public, true) {
		t.Fatal("the first update is a change")
	}
	s.mu.Lock()
	got := s.Peers[peer].Candidates()
	s.mu.Unlock()
	if len(got) != 2 || got[0] != public || got[1] != lan {
		t.Errorf("observed should come first: got %v", got)
	}
	// Repeating the same information is not a change.
	if s.UpdateEndpoints(peer, []netip.AddrPort{lan}, public, true) {
		t.Error("an unchanged update should report false")
	}
}

func TestRelayRegistrationNeedsTheRightToken(t *testing.T) {
	s := quietState(0)
	peer, _ := s.RegisterPeer("a", proto.PublicKey{1})
	addr := netip.MustParseAddrPort("203.0.113.7:41234")

	var wrong proto.RelayToken
	for i := range wrong {
		wrong[i] = 0xFF
	}
	if s.RegisterRelay(peer.ID, addr, wrong) {
		t.Error("a forged token must be refused")
	}
	if !s.RegisterRelay(peer.ID, addr, peer.RelayToken) {
		t.Error("the issued token must be accepted")
	}
}

func TestRelayRefusesPeersWithNoNetworkInCommon(t *testing.T) {
	s := quietState(0)
	alice, _ := s.RegisterPeer("alice", proto.PublicKey{1})
	bob, _ := s.RegisterPeer("bob", proto.PublicKey{2})
	stranger, _ := s.RegisterPeer("stranger", proto.PublicKey{3})

	id := s.CreateNetwork("games", "pw", proto.PublicKey{1})
	s.JoinNetwork(alice.ID, id, "pw")
	s.JoinNetwork(bob.ID, id, "pw")

	aliceAddr := netip.MustParseAddrPort("203.0.113.1:1000")
	bobAddr := netip.MustParseAddrPort("203.0.113.2:1000")
	strangerAddr := netip.MustParseAddrPort("203.0.113.3:1000")
	s.RegisterRelay(alice.ID, aliceAddr, alice.RelayToken)
	s.RegisterRelay(bob.ID, bobAddr, bob.RelayToken)
	s.RegisterRelay(stranger.ID, strangerAddr, stranger.RelayToken)

	if _, _, ok := s.RelayTarget(aliceAddr, bob.ID); !ok {
		t.Error("peers sharing a network should be relayed")
	}
	if _, _, ok := s.RelayTarget(strangerAddr, bob.ID); ok {
		t.Error("a stranger must not be relayed to bob")
	}
}

func TestNetworkIDsDoNotCollide(t *testing.T) {
	s := quietState(0)
	seen := map[string]bool{}
	for i := 0; i < 500; i++ {
		id := s.CreateNetwork("n", "pw", proto.PublicKey{1})
		if seen[id] {
			t.Fatalf("duplicate network id %s", id)
		}
		seen[id] = true
	}
}

func TestPasswordHashIsSaltedSoEqualPasswordsDiffer(t *testing.T) {
	s := quietState(0)
	a := s.CreateNetwork("one", "same", proto.PublicKey{1})
	b := s.CreateNetwork("two", "same", proto.PublicKey{1})
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.Networks[a].PasswordHash == s.Networks[b].PasswordHash {
		t.Error("identical passwords must hash differently under different salts")
	}
	if _, err := hex.DecodeString(s.Networks[a].Salt); err != nil {
		t.Errorf("salt should be hex: %v", err)
	}
}
