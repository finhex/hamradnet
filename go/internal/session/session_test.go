package session

import (
	"bytes"
	"testing"
)

// connected drives a full handshake and hands back both sides.
func connected(t *testing.T) (*Session, *Session) {
	t.Helper()
	alicePriv, _, err := GenerateKeypair()
	if err != nil {
		t.Fatal(err)
	}
	bobPriv, bobPub, err := GenerateKeypair()
	if err != nil {
		t.Fatal(err)
	}

	initiator, msg1, err := Initiate(alicePriv, bobPub, 111)
	if err != nil {
		t.Fatal(err)
	}
	responder, err := Respond(bobPriv, msg1)
	if err != nil {
		t.Fatal(err)
	}
	bob, msg2, err := responder.Finish(222, 111)
	if err != nil {
		t.Fatal(err)
	}
	alice, err := initiator.Finish(msg2, 222)
	if err != nil {
		t.Fatal(err)
	}
	return alice, bob
}

func TestPublicKeyDerivationMatchesGeneration(t *testing.T) {
	private, public, err := GenerateKeypair()
	if err != nil {
		t.Fatal(err)
	}
	derived, err := PublicFromPrivate(private)
	if err != nil {
		t.Fatal(err)
	}
	if derived != public {
		t.Errorf("derived %x, generated %x", derived, public)
	}
}

func TestHandshakeAgreesOnIdentitiesAndIndexes(t *testing.T) {
	alicePriv, alicePub, _ := GenerateKeypair()
	bobPriv, bobPub, _ := GenerateKeypair()

	initiator, msg1, err := Initiate(alicePriv, bobPub, 111)
	if err != nil {
		t.Fatal(err)
	}
	responder, err := Respond(bobPriv, msg1)
	if err != nil {
		t.Fatal(err)
	}
	// Bob learns who is calling before he answers.
	if responder.RemotePublic != alicePub {
		t.Fatalf("responder saw %x, expected %x", responder.RemotePublic, alicePub)
	}

	bob, msg2, err := responder.Finish(222, 111)
	if err != nil {
		t.Fatal(err)
	}
	alice, err := initiator.Finish(msg2, 222)
	if err != nil {
		t.Fatal(err)
	}

	if alice.RemotePublic != bobPub {
		t.Error("the initiator should know the responder's key")
	}
	if alice.LocalIndex != 111 || alice.RemoteIndex != 222 {
		t.Errorf("alice indexes: %d/%d", alice.LocalIndex, alice.RemoteIndex)
	}
	if bob.LocalIndex != 222 || bob.RemoteIndex != 111 {
		t.Errorf("bob indexes: %d/%d", bob.LocalIndex, bob.RemoteIndex)
	}
}

func TestTrafficFlowsInBothDirections(t *testing.T) {
	alice, bob := connected(t)

	counter, wire, err := alice.Encrypt([]byte("hello bob"))
	if err != nil {
		t.Fatal(err)
	}
	got, err := bob.Decrypt(counter, wire)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != "hello bob" {
		t.Errorf("got %q", got)
	}

	counter, wire, err = bob.Encrypt([]byte("hello alice"))
	if err != nil {
		t.Fatal(err)
	}
	got, err = alice.Decrypt(counter, wire)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != "hello alice" {
		t.Errorf("got %q", got)
	}
}

func TestCiphertextIsNotPlaintext(t *testing.T) {
	alice, _ := connected(t)
	_, wire, err := alice.Encrypt([]byte("secret payload"))
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Contains(wire, []byte("secret")) {
		t.Error("the payload appears in the clear")
	}
}

func TestOutOfOrderDeliveryIsAccepted(t *testing.T) {
	alice, bob := connected(t)

	type packet struct {
		counter uint64
		wire    []byte
	}
	var packets []packet
	for i := 0; i < 8; i++ {
		counter, wire, err := alice.Encrypt([]byte{byte(i)})
		if err != nil {
			t.Fatal(err)
		}
		packets = append(packets, packet{counter, wire})
	}
	// Deliver them backwards.
	for i := len(packets) - 1; i >= 0; i-- {
		if _, err := bob.Decrypt(packets[i].counter, packets[i].wire); err != nil {
			t.Fatalf("packet %d rejected: %v", i, err)
		}
	}
}

func TestReplayedPacketIsRejected(t *testing.T) {
	alice, bob := connected(t)
	counter, wire, _ := alice.Encrypt([]byte("once"))
	if _, err := bob.Decrypt(counter, wire); err != nil {
		t.Fatal(err)
	}
	if _, err := bob.Decrypt(counter, wire); err == nil {
		t.Error("a replay must be rejected")
	}
}

func TestTamperedCiphertextIsRejected(t *testing.T) {
	alice, bob := connected(t)
	counter, wire, _ := alice.Encrypt([]byte("important"))
	wire[0] ^= 0x01
	if _, err := bob.Decrypt(counter, wire); err == nil {
		t.Error("a tampered packet must be rejected")
	}
}

func TestWrongCounterIsRejected(t *testing.T) {
	alice, bob := connected(t)
	_, wire, _ := alice.Encrypt([]byte("important"))
	if _, err := bob.Decrypt(9999, wire); err == nil {
		t.Error("the wrong counter must not verify")
	}
}

// A forged packet must not be able to advance the replay window and lock out
// the genuine traffic that follows it.
func TestForgedPacketDoesNotStarveTheSession(t *testing.T) {
	alice, bob := connected(t)
	forged := make([]byte, 32)
	if _, err := bob.Decrypt(^uint64(0)-1, forged); err == nil {
		t.Fatal("a forged packet should not verify")
	}

	counter, wire, _ := alice.Encrypt([]byte("genuine"))
	got, err := bob.Decrypt(counter, wire)
	if err != nil {
		t.Fatalf("genuine traffic must still arrive: %v", err)
	}
	if string(got) != "genuine" {
		t.Errorf("got %q", got)
	}
}

func TestHandshakeToTheWrongKeyFails(t *testing.T) {
	alicePriv, _, _ := GenerateKeypair()
	malloryPriv, _, _ := GenerateKeypair()
	_, wrongPub, _ := GenerateKeypair()

	// Alice dials a key nobody holds the private half of.
	_, msg1, err := Initiate(alicePriv, wrongPub, 1)
	if err != nil {
		t.Fatal(err)
	}
	// Mallory cannot read a handshake addressed to somebody else.
	if _, err := Respond(malloryPriv, msg1); err == nil {
		t.Error("an unaddressed handshake must not be readable")
	}
}

func TestTwoSessionsDoNotShareKeys(t *testing.T) {
	aliceA, _ := connected(t)
	_, bobB := connected(t)
	counter, wire, _ := aliceA.Encrypt([]byte("cross talk"))
	if _, err := bobB.Decrypt(counter, wire); err == nil {
		t.Error("a session must not open another session's traffic")
	}
}

func TestEmptyPayloadRoundTrips(t *testing.T) {
	// Keepalives carry no payload at all.
	alice, bob := connected(t)
	counter, wire, err := alice.Encrypt(nil)
	if err != nil {
		t.Fatal(err)
	}
	got, err := bob.Decrypt(counter, wire)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 0 {
		t.Errorf("expected an empty payload, got %d bytes", len(got))
	}
}
