// Package session is the end-to-end encryption between two clients.
//
// The pattern is Noise_IK_25519_ChaChaPoly_BLAKE2s, the same one WireGuard uses
// and the same one the Rust implementation uses, so the two interoperate. IK
// fits this design exactly: the initiator already knows the responder's static
// public key, because the coordinator published it in the network roster, so a
// tunnel comes up in one round trip.
//
// Transport supplies the nonce per message rather than letting the cipher keep
// its own counter. That is what makes UDP workable: the counter travels in the
// packet header, so reordered and dropped datagrams do not desynchronise
// anything.
package session

import (
	"crypto/rand"
	"encoding/binary"
	"errors"
	"fmt"
	"sync"
	"time"

	"github.com/flynn/noise"
	"github.com/finhex/hamradnet/internal/proto"
)

// KeyLen is the length of an X25519 key.
const KeyLen = 32

// tagLen is the ChaCha20-Poly1305 authentication tag.
const tagLen = 16

func cipherSuite() noise.CipherSuite {
	return noise.NewCipherSuite(noise.DH25519, noise.CipherChaChaPoly, noise.HashBLAKE2s)
}

// GenerateKeypair makes a fresh identity.
func GenerateKeypair() (private, public [KeyLen]byte, err error) {
	kp, err := cipherSuite().GenerateKeypair(rand.Reader)
	if err != nil {
		return private, public, err
	}
	copy(private[:], kp.Private)
	copy(public[:], kp.Public)
	return private, public, nil
}

// PublicFromPrivate derives the public half of an identity key.
func PublicFromPrivate(private [KeyLen]byte) ([KeyLen]byte, error) {
	var public [KeyLen]byte
	suite := cipherSuite()
	// DH of the private key with the curve base point is the public key.
	pub, err := suite.DH(private[:], basePoint[:])
	if err != nil {
		return public, err
	}
	copy(public[:], pub)
	return public, nil
}

// basePoint is the X25519 generator, u = 9.
var basePoint = [32]byte{9}

// RandomIndex picks a local session index, echoed back by the peer so inbound
// packets can be matched to a session without a linear scan.
func RandomIndex() uint32 {
	var b [4]byte
	_, _ = rand.Read(b[:])
	return binary.BigEndian.Uint32(b[:])
}

// Initiator is a handshake we started.
type Initiator struct {
	state      *noise.HandshakeState
	LocalIndex uint32
	Started    time.Time
}

// Initiate begins a handshake, returning the first Noise message to send.
func Initiate(private, remotePublic [KeyLen]byte, localIndex uint32) (*Initiator, []byte, error) {
	public, err := PublicFromPrivate(private)
	if err != nil {
		return nil, nil, err
	}
	hs, err := noise.NewHandshakeState(noise.Config{
		CipherSuite:   cipherSuite(),
		Random:        rand.Reader,
		Pattern:       noise.HandshakeIK,
		Initiator:     true,
		StaticKeypair: noise.DHKey{Private: private[:], Public: public[:]},
		PeerStatic:    remotePublic[:],
	})
	if err != nil {
		return nil, nil, fmt.Errorf("building the Noise initiator: %w", err)
	}
	msg, _, _, err := hs.WriteMessage(nil, nil)
	if err != nil {
		return nil, nil, err
	}
	return &Initiator{state: hs, LocalIndex: localIndex, Started: time.Now()}, msg, nil
}

// Finish consumes the peer's reply and promotes the handshake to a live session.
func (i *Initiator) Finish(reply []byte, remoteIndex uint32) (*Session, error) {
	_, cs0, cs1, err := i.state.ReadMessage(nil, reply)
	if err != nil {
		return nil, fmt.Errorf("reading the handshake response: %w", err)
	}
	if cs0 == nil || cs1 == nil {
		return nil, errors.New("handshake did not complete")
	}
	var remotePublic [KeyLen]byte
	copy(remotePublic[:], i.state.PeerStatic())
	// The initiator sends with the first cipher state and receives with the
	// second; the responder has them the other way round.
	return &Session{
		send:         cs0,
		recv:         cs1,
		LocalIndex:   i.LocalIndex,
		RemoteIndex:  remoteIndex,
		RemotePublic: remotePublic,
		Established:  time.Now(),
	}, nil
}

// Responder is a handshake somebody started with us, read but not yet answered.
//
// Deliberately split in two: the caller gets to check RemotePublic against the
// roster and drop the handshake before we ever answer a stranger.
type Responder struct {
	state        *noise.HandshakeState
	RemotePublic [KeyLen]byte
}

// Respond reads an initiation without answering it.
func Respond(private [KeyLen]byte, initial []byte) (*Responder, error) {
	public, err := PublicFromPrivate(private)
	if err != nil {
		return nil, err
	}
	hs, err := noise.NewHandshakeState(noise.Config{
		CipherSuite:   cipherSuite(),
		Random:        rand.Reader,
		Pattern:       noise.HandshakeIK,
		Initiator:     false,
		StaticKeypair: noise.DHKey{Private: private[:], Public: public[:]},
	})
	if err != nil {
		return nil, fmt.Errorf("building the Noise responder: %w", err)
	}
	if _, _, _, err := hs.ReadMessage(nil, initial); err != nil {
		return nil, fmt.Errorf("reading the handshake initiation: %w", err)
	}
	var remote [KeyLen]byte
	peer := hs.PeerStatic()
	if len(peer) != KeyLen {
		return nil, errors.New("peer did not present a static key")
	}
	copy(remote[:], peer)
	return &Responder{state: hs, RemotePublic: remote}, nil
}

// Finish produces the reply and the live session.
func (r *Responder) Finish(localIndex, remoteIndex uint32) (*Session, []byte, error) {
	reply, cs0, cs1, err := r.state.WriteMessage(nil, nil)
	if err != nil {
		return nil, nil, err
	}
	if cs0 == nil || cs1 == nil {
		return nil, nil, errors.New("handshake did not complete")
	}
	return &Session{
		send:         cs1,
		recv:         cs0,
		LocalIndex:   localIndex,
		RemoteIndex:  remoteIndex,
		RemotePublic: r.RemotePublic,
		Established:  time.Now(),
	}, reply, nil
}

// Session is an established tunnel to one peer.
type Session struct {
	mu   sync.Mutex
	send *noise.CipherState
	recv *noise.CipherState

	LocalIndex   uint32
	RemoteIndex  uint32
	RemotePublic [KeyLen]byte
	Established  time.Time

	txCounter uint64
	replay    proto.ReplayWindow
}

// ErrExhausted means the nonce space ran out and the session must be replaced.
var ErrExhausted = errors.New("nonce space exhausted; the session must be rekeyed")

// Encrypt seals one packet, returning the counter to put in its header.
func (s *Session) Encrypt(plaintext []byte) (uint64, []byte, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	counter := s.txCounter
	if counter == ^uint64(0) {
		return 0, nil, ErrExhausted
	}
	s.txCounter++

	s.send.SetNonce(counter)
	out, err := s.send.Encrypt(nil, nil, plaintext)
	if err != nil {
		return 0, nil, err
	}
	return counter, out, nil
}

// Decrypt opens one packet.
//
// The replay window is only advanced after the tag verifies. The other way round
// would let anyone who can spoof a datagram burn a counter far ahead of the real
// one and starve the session of legitimate packets.
func (s *Session) Decrypt(counter uint64, ciphertext []byte) ([]byte, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	if len(ciphertext) < tagLen {
		return nil, errors.New("ciphertext shorter than the authentication tag")
	}
	s.recv.SetNonce(counter)
	plaintext, err := s.recv.Decrypt(nil, nil, ciphertext)
	if err != nil {
		return nil, errors.New("packet failed authentication")
	}
	if !s.replay.Accept(counter) {
		return nil, fmt.Errorf("replayed or stale packet (counter %d)", counter)
	}
	return plaintext, nil
}
