package client

import (
	"encoding/json"
	"strings"
	"testing"
)

// Frames exactly as the Rust daemon and CLI write them (serde_json, internally
// tagged enums, Option as null). If these stop decoding, a Go command line can
// no longer drive a Rust daemon, or the other way round.
const (
	rustStatusReply = `{"reply":"status","status":{"connected":true,"server":"203.0.113.45:7777",` +
		`"name":"alice","peer_id":3,"virtual_ip":"10.66.0.3","public_endpoint":null,` +
		`"networks":[{"network_id":"HAM-1111-2222","name":"game night"}],"peer_count":1,` +
		`"direct_count":0,"server_rtt_ms":null,"total_rx":10,"total_tx":20,"last_error":null}}`
	rustDisconnectedStatus = `{"reply":"status","status":{"connected":false,"server":"x:7777",` +
		`"name":"alice","peer_id":null,"virtual_ip":null,"public_endpoint":null,"networks":[],` +
		`"peer_count":0,"direct_count":0,"server_rtt_ms":null,"total_rx":0,"total_tx":0,` +
		`"last_error":"connection refused"}}`
	rustPeersReply = `{"reply":"peers","peers":[{"peer_id":2,"name":"bob","virtual_ip":"10.66.0.2",` +
		`"link":"up","path":"relay","endpoint":null,"rx_bytes":5,"tx_bytes":6,` +
		`"last_seen_secs":1,"rtt_ms":23.5,"networks":["HAM-1111-2222"]}]}`
	rustErrorReply = `{"reply":"error","message":"bad password"}`
	rustJoin       = `{"token":"t","command":{"cmd":"join_network","network_id":"HAM-1-2","password":"p"}}`
)

func TestDecodesWhatTheRustDaemonSends(t *testing.T) {
	var status Response
	if err := json.Unmarshal([]byte(rustStatusReply), &status); err != nil {
		t.Fatal(err)
	}
	s := status.Status
	if !status.OK || s == nil || s.VirtualIP != "10.66.0.3" || s.PeerID != 3 ||
		s.ServerRTTMs != 0 || len(s.Networks) != 1 {
		t.Fatalf("status decoded wrongly: %+v %+v", status, s)
	}

	var down Response
	if err := json.Unmarshal([]byte(rustDisconnectedStatus), &down); err != nil {
		t.Fatal(err)
	}
	if down.Status.VirtualIP != "" || down.Status.LastError != "connection refused" {
		t.Fatalf("nulls decoded wrongly: %+v", down.Status)
	}

	var peers Response
	if err := json.Unmarshal([]byte(rustPeersReply), &peers); err != nil {
		t.Fatal(err)
	}
	if len(peers.Peers) != 1 || peers.Peers[0].RTTMs != 23.5 || peers.Peers[0].Path != "relay" {
		t.Fatalf("peers decoded wrongly: %+v", peers.Peers)
	}

	var failure Response
	if err := json.Unmarshal([]byte(rustErrorReply), &failure); err != nil {
		t.Fatal(err)
	}
	if failure.OK || failure.Message != "bad password" {
		t.Fatalf("error decoded wrongly: %+v", failure)
	}
}

func TestDecodesWhatTheRustCommandLineSends(t *testing.T) {
	var req Request
	if err := json.Unmarshal([]byte(rustJoin), &req); err != nil {
		t.Fatal(err)
	}
	if req.Command != "join" || req.NetworkID != "HAM-1-2" || req.Password != "p" || req.Token != "t" {
		t.Fatalf("request decoded wrongly: %+v", req)
	}
}

// Rust rejects null for a Vec and "" for an address, so neither may appear.
func TestEncodesWhatTheRustSideAccepts(t *testing.T) {
	for _, c := range []struct {
		name string
		resp Response
		want []string
		bad  []string
	}{
		{"fresh status", Response{OK: true, Status: &Status{Server: "x:7777"}},
			[]string{`"reply":"status"`, `"networks":[]`, `"virtual_ip":null`, `"peer_id":null`},
			[]string{`"virtual_ip":""`, `null,"peer_count"`}},
		{"connected status", Response{OK: true, Status: &Status{VirtualIP: "10.66.0.2", PeerID: 4, ServerRTTMs: 12}},
			[]string{`"virtual_ip":"10.66.0.2"`, `"peer_id":4`, `"server_rtt_ms":12`}, nil},
		{"no peers", Response{OK: true, Peers: []PeerReport{}},
			[]string{`"reply":"peers"`, `"peers":[]`}, nil},
		{"unmeasured peer", Response{OK: true, Peers: []PeerReport{{PeerID: 2, VirtualIP: "10.66.0.2"}}},
			[]string{`"rtt_ms":null`, `"networks":[]`}, []string{`"rtt_ms":0`}},
		{"ok", Response{OK: true, Message: "done"}, []string{`"reply":"ok"`, `"message":"done"`}, nil},
		{"error", Response{Message: "nope"}, []string{`"reply":"error"`}, nil},
	} {
		raw, err := json.Marshal(c.resp)
		if err != nil {
			t.Fatalf("%s: %v", c.name, err)
		}
		for _, want := range c.want {
			if !strings.Contains(string(raw), want) {
				t.Errorf("%s: %s lacks %s", c.name, raw, want)
			}
		}
		for _, bad := range c.bad {
			if strings.Contains(string(raw), bad) {
				t.Errorf("%s: %s contains %s", c.name, raw, bad)
			}
		}
	}

	raw, _ := json.Marshal(Request{Token: "t", Command: "create", Name: "n", Password: "p"})
	if string(raw) != `{"token":"t","command":{"cmd":"create_network","name":"n","password":"p"}}` {
		t.Errorf("request encoded as %s", raw)
	}
}

// The Go command line and daemon talk to each other through the same encoding,
// so every command and reply has to survive the round trip unchanged.
func TestRoundTripsThroughTheWireFormat(t *testing.T) {
	for _, req := range []Request{
		{Token: "t", Command: "status"},
		{Token: "t", Command: "create", Name: "n", Password: "p"},
		{Token: "t", Command: "join", NetworkID: "HAM-1-2", Password: "p"},
		{Token: "t", Command: "leave", NetworkID: "HAM-1-2"},
		{Token: "t", Command: "delete", NetworkID: "HAM-1-2"},
	} {
		raw, _ := json.Marshal(req)
		var back Request
		if err := json.Unmarshal(raw, &back); err != nil || back != req {
			t.Errorf("%+v came back as %+v (%v)", req, back, err)
		}
	}

	status := Status{Connected: true, Server: "s", Name: "n", PeerID: 5, VirtualIP: "10.66.0.5",
		PublicAddr: "203.0.113.1:4000", Networks: []NetworkReport{{NetworkID: "HAM-1-2", Name: "x"}},
		PeerCount: 2, DirectCount: 1, ServerRTTMs: 31, TotalRX: 1, TotalTX: 2, LastError: "e"}
	raw, _ := json.Marshal(Response{OK: true, Status: &status})
	var back Response
	if err := json.Unmarshal(raw, &back); err != nil {
		t.Fatal(err)
	}
	if got, _ := json.Marshal(back.Status); string(got) != string(mustMarshal(t, status)) {
		t.Errorf("status came back as %s", got)
	}
}

func mustMarshal(t *testing.T, v any) []byte {
	t.Helper()
	raw, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	return raw
}
