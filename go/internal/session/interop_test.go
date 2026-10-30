package session

import (
	"bufio"
	"encoding/hex"
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"testing"
)

// TestInteropWithRustResponder drives a real handshake against the Rust
// implementation and exchanges traffic both ways.
//
// Both sides implement Noise_IK_25519_ChaChaPoly_BLAKE2s, but "both implement
// the same spec" is not the same as "both interoperate": getting the two cipher
// states the wrong way round, or numbering the transport nonces differently,
// would still pass every test each implementation runs against itself. This
// catches that.
//
// Skipped unless HAMRADNET_RUST_RESPONDER points at the helper binary, so the
// Go suite stays self-contained:
//
//	(cd rust && cargo build --release --example noise_responder -p hamradnet)
//	HAMRADNET_RUST_RESPONDER=$PWD/rust/target/release/examples/noise_responder \
//	    sh -c 'cd go && go test ./internal/session/'
func TestInteropWithRustResponder(t *testing.T) {
	helper := os.Getenv("HAMRADNET_RUST_RESPONDER")
	if helper == "" {
		t.Skip("set HAMRADNET_RUST_RESPONDER to the Rust helper binary to run this")
	}

	// The Rust side plays the responder, so it needs a key we also know the
	// public half of.
	bobPriv, bobPub, err := GenerateKeypair()
	if err != nil {
		t.Fatal(err)
	}
	alicePriv, _, err := GenerateKeypair()
	if err != nil {
		t.Fatal(err)
	}

	cmd := exec.Command(helper, hex.EncodeToString(bobPriv[:]))
	stdin, err := cmd.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	cmd.Stderr = os.Stderr
	if err := cmd.Start(); err != nil {
		t.Fatalf("starting the Rust helper: %v", err)
	}
	defer func() {
		_ = stdin.Close()
		_ = cmd.Wait()
	}()
	replies := bufio.NewScanner(stdout)

	readLine := func(what string) string {
		if !replies.Scan() {
			t.Fatalf("the Rust helper produced no %s", what)
		}
		return strings.TrimSpace(replies.Text())
	}

	// --- Go initiates, Rust answers. ---
	initiator, msg1, err := Initiate(alicePriv, bobPub, 111)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := fmt.Fprintf(stdin, "%s\n", hex.EncodeToString(msg1)); err != nil {
		t.Fatal(err)
	}

	msg2, err := hex.DecodeString(readLine("handshake reply"))
	if err != nil {
		t.Fatalf("reply was not hex: %v", err)
	}
	alice, err := initiator.Finish(msg2, 222)
	if err != nil {
		t.Fatalf("Go could not complete a handshake Rust answered: %v", err)
	}
	if alice.RemotePublic != bobPub {
		t.Errorf("wrong remote key: got %x", alice.RemotePublic)
	}

	// --- Go -> Rust. ---
	const secret = "ping from go"
	counter, ciphertext, err := alice.Encrypt([]byte(secret))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := fmt.Fprintf(stdin, "%d %s\n", counter, hex.EncodeToString(ciphertext)); err != nil {
		t.Fatal(err)
	}

	decrypted, err := hex.DecodeString(readLine("decrypted payload"))
	if err != nil {
		t.Fatal(err)
	}
	if string(decrypted) != secret {
		t.Errorf("Rust decrypted %q, expected %q", decrypted, secret)
	}

	// --- Rust -> Go. ---
	parts := strings.SplitN(readLine("reply packet"), " ", 2)
	if len(parts) != 2 {
		t.Fatalf("malformed reply packet: %v", parts)
	}
	replyCounter, err := strconv.ParseUint(parts[0], 10, 64)
	if err != nil {
		t.Fatal(err)
	}
	replyCipher, err := hex.DecodeString(parts[1])
	if err != nil {
		t.Fatal(err)
	}
	plaintext, err := alice.Decrypt(replyCounter, replyCipher)
	if err != nil {
		t.Fatalf("Go could not decrypt what Rust sent: %v", err)
	}
	if string(plaintext) != "pong from rust" {
		t.Errorf("got %q", plaintext)
	}
}
