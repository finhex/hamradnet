package proto

import (
	"bytes"
	"encoding/json"
	"math/rand"
	"net/netip"
	"testing"
)

func roundTrip(t *testing.T, p Packet) {
	t.Helper()
	encoded := p.Encode(nil)
	decoded, err := Decode(encoded)
	if err != nil {
		t.Fatalf("decoding %v: %v", p.Type, err)
	}
	if decoded.Type != p.Type {
		t.Fatalf("type: got %v, want %v", decoded.Type, p.Type)
	}
}

func TestPacketsRoundTrip(t *testing.T) {
	cases := []Packet{
		{Type: TypeHandshakeInit, SenderIndex: 9, Noise: []byte{1, 2, 3}},
		{Type: TypeHandshakeResp, SenderIndex: 9, ReceiverIndex: 4, Noise: []byte{5, 6}},
		{Type: TypeData, ReceiverIndex: 3, Counter: ^uint64(0), Ciphertext: bytes.Repeat([]byte{9}, 40)},
		{Type: TypePunch, Sender: 5},
		{Type: TypeRelayForward, Dst: 77, Inner: []byte{0x03, 0, 0}},
		{Type: TypeRelayDeliver, Src: 77, Inner: []byte{0x03, 0, 0}},
		{Type: TypeReflectRequest, TxID: 12345},
	}
	for _, c := range cases {
		roundTrip(t, c)
	}
}

func TestDataPacketPreservesEveryField(t *testing.T) {
	original := Packet{
		Type:          TypeData,
		ReceiverIndex: 0xDEADBEEF,
		Counter:       0x0102030405060708,
		Ciphertext:    []byte("some ciphertext"),
	}
	decoded, err := Decode(original.Encode(nil))
	if err != nil {
		t.Fatal(err)
	}
	if decoded.ReceiverIndex != original.ReceiverIndex {
		t.Errorf("receiver index: got %d", decoded.ReceiverIndex)
	}
	if decoded.Counter != original.Counter {
		t.Errorf("counter: got %d", decoded.Counter)
	}
	if !bytes.Equal(decoded.Ciphertext, original.Ciphertext) {
		t.Errorf("ciphertext: got %q", decoded.Ciphertext)
	}
}

func TestReflectResponseRoundTripsBothFamilies(t *testing.T) {
	for _, text := range []string{"203.0.113.9:41234", "[2001:db8::1]:41234"} {
		addr := netip.MustParseAddrPort(text)
		encoded := Packet{Type: TypeReflectResponse, TxID: 7, Observed: addr}.Encode(nil)
		decoded, err := Decode(encoded)
		if err != nil {
			t.Fatalf("%s: %v", text, err)
		}
		if decoded.Observed != addr {
			t.Errorf("%s: got %v", text, decoded.Observed)
		}
		if decoded.TxID != 7 {
			t.Errorf("%s: txid got %d", text, decoded.TxID)
		}
	}
}

func TestRelayRegisterKeepsItsToken(t *testing.T) {
	var token [RelayTokenLen]byte
	for i := range token {
		token[i] = byte(i)
	}
	decoded, err := Decode(Packet{Type: TypeRelayRegister, PeerID: 42, Token: token}.Encode(nil))
	if err != nil {
		t.Fatal(err)
	}
	if decoded.PeerID != 42 || decoded.Token != token {
		t.Errorf("got peer %d token %x", decoded.PeerID, decoded.Token)
	}
}

func TestTruncatedPacketsAreRejectedNotPanicking(t *testing.T) {
	for n := 0; n < 13; n++ {
		buf := bytes.Repeat([]byte{0x03}, n)
		if _, err := Decode(buf); err == nil {
			t.Errorf("length %d should have been rejected", n)
		}
	}
}

func TestUnknownTypeIsRejected(t *testing.T) {
	if _, err := Decode([]byte{0xff, 0, 0, 0, 0}); err == nil {
		t.Error("expected an error")
	}
}

func TestReplayWindow(t *testing.T) {
	t.Run("accepts in order", func(t *testing.T) {
		var w ReplayWindow
		for i := uint64(1); i < 1000; i++ {
			if !w.Accept(i) {
				t.Fatalf("counter %d should be fresh", i)
			}
		}
	})
	t.Run("rejects repeats", func(t *testing.T) {
		var w ReplayWindow
		if !w.Accept(5) || w.Accept(5) {
			t.Error("a repeat must be rejected")
		}
		if !w.Accept(4) || w.Accept(4) {
			t.Error("a repeat below the mark must be rejected")
		}
	})
	t.Run("accepts reordering but not ancient", func(t *testing.T) {
		var w ReplayWindow
		for _, c := range []uint64{100, 99, 60} {
			if !w.Accept(c) {
				t.Fatalf("%d should be accepted", c)
			}
		}
		// 100 - 64 = 36, so anything at or below is outside the window.
		for _, c := range []uint64{36, 1} {
			if w.Accept(c) {
				t.Errorf("%d is too old and should be rejected", c)
			}
		}
	})
}

// The JSON has to match what serde produces on the Rust side exactly, or the
// two implementations cannot talk to each other.
func TestHelloDecodesRustJSON(t *testing.T) {
	// Captured from the Rust client.
	raw := `{"t":"hello","version":1,"public_key":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=","name":"konako"}`
	msg, err := UnmarshalClientMsg([]byte(raw))
	if err != nil {
		t.Fatal(err)
	}
	if msg.T != MsgHello || msg.Version != 1 || msg.Name != "konako" {
		t.Fatalf("got %+v", msg)
	}
	for _, b := range msg.PublicKey {
		if b != 7 {
			t.Fatalf("public key did not decode: %x", msg.PublicKey)
		}
	}
}

func TestClientMessagesMissingFieldsAreRejected(t *testing.T) {
	for _, raw := range []string{
		`{"t":"hello","version":1}`,
		`{"t":"join_network","network_id":"HAM-1-2"}`,
		`{"t":"create_network","name":"x"}`,
		`{"t":"leave_network"}`,
		`{"t":"nonsense"}`,
	} {
		if _, err := UnmarshalClientMsg([]byte(raw)); err == nil {
			t.Errorf("%s should have been rejected", raw)
		}
	}
}

func TestEmptyPasswordSurvivesTheRoundTrip(t *testing.T) {
	// An empty string is a real value here, not an absent field; dropping it
	// would make the Rust side reject the frame as incomplete.
	msg, err := UnmarshalClientMsg([]byte(`{"t":"join_network","network_id":"HAM-1-2","password":""}`))
	if err != nil {
		t.Fatal(err)
	}
	if msg.NetworkID != "HAM-1-2" || msg.Password != "" {
		t.Fatalf("got %+v", msg)
	}
}

func TestServerMessageShapeMatchesSerde(t *testing.T) {
	encoded, err := MarshalServerMsg(MsgWelcome, WelcomeBody{
		Version:   1,
		PeerID:    42,
		VirtualIP: netip.MustParseAddr("10.66.0.2"),
		PrefixLen: 16,
		UDPPort:   7778,
		RelayToken: RelayToken{0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab,
			0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab},
	})
	if err != nil {
		t.Fatal(err)
	}

	var got map[string]any
	if err := json.Unmarshal(encoded, &got); err != nil {
		t.Fatal(err)
	}
	// The tag sits flat beside the variant's own fields, not nested.
	if got["t"] != "welcome" {
		t.Errorf("tag: got %v", got["t"])
	}
	if got["virtual_ip"] != "10.66.0.2" {
		t.Errorf("virtual_ip should serialise as a plain string, got %v", got["virtual_ip"])
	}
	if got["relay_token"] != "abababababababababababababababab" {
		t.Errorf("relay_token should be hex, got %v", got["relay_token"])
	}
	if got["udp_port"].(float64) != 7778 {
		t.Errorf("udp_port: got %v", got["udp_port"])
	}
}

func TestPongCarriesNothingButItsTag(t *testing.T) {
	encoded, err := MarshalServerMsg(MsgPong, nil)
	if err != nil {
		t.Fatal(err)
	}
	if string(encoded) != `{"t":"pong"}` {
		t.Errorf("got %s", encoded)
	}
}

func TestPeerInfoSerialisesTheWayRustExpects(t *testing.T) {
	encoded, err := json.Marshal(PeerInfo{
		PeerID:    7,
		Name:      "alice",
		PublicKey: PublicKey{1, 2, 3},
		VirtualIP: netip.MustParseAddr("10.66.0.7"),
		Endpoints: []netip.AddrPort{netip.MustParseAddrPort("203.0.113.9:7000")},
	})
	if err != nil {
		t.Fatal(err)
	}
	var got map[string]any
	if err := json.Unmarshal(encoded, &got); err != nil {
		t.Fatal(err)
	}
	if got["virtual_ip"] != "10.66.0.7" {
		t.Errorf("virtual_ip: got %v", got["virtual_ip"])
	}
	endpoints := got["endpoints"].([]any)
	if len(endpoints) != 1 || endpoints[0] != "203.0.113.9:7000" {
		t.Errorf("endpoints: got %v", endpoints)
	}
}

func TestFrameRoundTrip(t *testing.T) {
	var buf bytes.Buffer
	payload := []byte(`{"t":"pong"}`)
	if err := WriteFrame(&buf, payload); err != nil {
		t.Fatal(err)
	}
	// Four bytes of length, then the body.
	if buf.Len() != 4+len(payload) {
		t.Fatalf("framed length: got %d", buf.Len())
	}
	got, err := ReadFrame(&buf)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, payload) {
		t.Errorf("got %s", got)
	}
}

func TestNetworkIDShape(t *testing.T) {
	rng := rand.New(rand.NewSource(1))
	id := GenerateNetworkID(rng)
	if len(id) != 13 || id[:4] != "HAM-" {
		t.Errorf("got %q", id)
	}
}
