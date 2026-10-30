//! A Noise IK responder on stdin/stdout, so another implementation can be
//! checked against this one.
//!
//! The Go client speaks the same pattern; if its handshake or its transport
//! nonces differed in any way, the exchange driven by this helper would fail.
//! Used by the Go test `TestInteropWithRustResponder`.
//!
//! Protocol on stdin, one hex line each:
//!   in : the initiator's first handshake message
//!   out: our reply
//!   in : `<counter> <ciphertext>` from the initiator
//!   out: the decrypted plaintext, hex
//!   out: `<counter> <ciphertext>` of a fixed reply, so the far side can check
//!        that traffic flows the other way too.

use std::io::{BufRead, Write};

fn from_hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    let private = from_hex(&std::env::args().nth(1).expect("private key as argv[1]"));

    let mut lines = std::io::stdin().lock().lines();
    let mut stdout = std::io::stdout();

    let mut handshake = snow::Builder::new(
        hamradnet::session::NOISE_PARAMS.parse().expect("params"),
    )
    .local_private_key(&private)
    .build_responder()
    .expect("responder");

    // Read the initiation and answer it.
    let msg1 = from_hex(&lines.next().expect("msg1").expect("stdin"));
    let mut scratch = vec![0u8; 1024];
    handshake.read_message(&msg1, &mut scratch).expect("read msg1");

    let mut msg2 = vec![0u8; 1024];
    let len = handshake.write_message(&[], &mut msg2).expect("write msg2");
    msg2.truncate(len);
    writeln!(stdout, "{}", to_hex(&msg2)).unwrap();
    stdout.flush().unwrap();

    let transport = handshake
        .into_stateless_transport_mode()
        .expect("transport mode");

    // Decrypt what the initiator sends.
    let line = lines.next().expect("data").expect("stdin");
    let (counter, ciphertext) = line.split_once(' ').expect("counter and ciphertext");
    let counter: u64 = counter.parse().expect("counter");
    let ciphertext = from_hex(ciphertext);

    let mut plaintext = vec![0u8; ciphertext.len()];
    let len = transport
        .read_message(counter, &ciphertext, &mut plaintext)
        .expect("decrypt");
    plaintext.truncate(len);
    writeln!(stdout, "{}", to_hex(&plaintext)).unwrap();

    // Send something back, so the other direction is exercised too.
    let reply = b"pong from rust";
    let mut out = vec![0u8; reply.len() + 16];
    let len = transport.write_message(0, reply, &mut out).expect("encrypt");
    out.truncate(len);
    writeln!(stdout, "0 {}", to_hex(&out)).unwrap();
    stdout.flush().unwrap();
}
