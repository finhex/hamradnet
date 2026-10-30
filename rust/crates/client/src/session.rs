//! End-to-end encryption between two clients.
//!
//! The pattern is `Noise_IK_25519_ChaChaPoly_BLAKE2s` - the same one WireGuard
//! uses. IK fits this design exactly: the initiator already knows the
//! responder's static public key, because the coordinator published it in the
//! network roster, so a tunnel comes up in one round trip.
//!
//! Transport uses snow's *stateless* mode, where the nonce is supplied per
//! message rather than tracked internally. That is what makes UDP workable: the
//! counter travels in the packet header, so reordered and dropped datagrams do
//! not desynchronise the cipher state.

use anyhow::{bail, Context, Result};
use hamradnet_proto::wire::ReplayWindow;
use rand::Rng;
use std::time::Instant;

pub const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// ChaCha20-Poly1305 authentication tag.
const TAG_LEN: usize = 16;
/// Comfortably larger than any Noise handshake message this pattern produces.
const HANDSHAKE_BUF: usize = 1024;

pub fn public_from_private(private: &[u8; 32]) -> Result<[u8; 32]> {
    let secret = x25519_dalek::StaticSecret::from(*private);
    Ok(x25519_dalek::PublicKey::from(&secret).to_bytes())
}

/// Local session index, echoed back by the peer so inbound packets can be
/// matched to a session without a linear scan.
pub fn random_index() -> u32 {
    rand::thread_rng().gen()
}

fn builder(private: &[u8; 32]) -> Result<snow::Builder<'_>> {
    Ok(snow::Builder::new(NOISE_PARAMS.parse()?).local_private_key(private))
}

/// An in-flight handshake we started.
pub struct Initiator {
    state: snow::HandshakeState,
    pub local_index: u32,
    pub started: Instant,
}

/// Begins a handshake, returning the first Noise message to send.
pub fn initiate(
    private: &[u8; 32],
    remote_public: &[u8; 32],
    local_index: u32,
) -> Result<(Initiator, Vec<u8>)> {
    let mut state = builder(private)?
        .remote_public_key(remote_public)
        .build_initiator()
        .context("building Noise initiator")?;
    let mut buf = vec![0u8; HANDSHAKE_BUF];
    let len = state.write_message(&[], &mut buf)?;
    buf.truncate(len);
    Ok((Initiator { state, local_index, started: Instant::now() }, buf))
}

impl Initiator {
    /// Consumes the peer's reply and promotes the handshake to a live session.
    pub fn finish(mut self, reply: &[u8], remote_index: u32) -> Result<Session> {
        let mut scratch = vec![0u8; HANDSHAKE_BUF];
        self.state
            .read_message(reply, &mut scratch)
            .context("reading handshake response")?;
        if !self.state.is_handshake_finished() {
            bail!("handshake did not complete");
        }
        let remote_public = remote_static(&self.state)?;
        Ok(Session {
            transport: self.state.into_stateless_transport_mode()?,
            local_index: self.local_index,
            remote_index,
            remote_public,
            tx_counter: 0,
            replay: ReplayWindow::default(),
            established: Instant::now(),
        })
    }
}

/// A handshake someone started with us, read but not yet answered.
///
/// Deliberately split in two: the caller gets to check `remote_public` against
/// the roster and drop the handshake before we ever answer a stranger.
pub struct Responder {
    state: snow::HandshakeState,
    pub remote_public: [u8; 32],
}

pub fn respond(private: &[u8; 32], initial: &[u8]) -> Result<Responder> {
    let mut state = builder(private)?
        .build_responder()
        .context("building Noise responder")?;
    let mut scratch = vec![0u8; HANDSHAKE_BUF];
    state
        .read_message(initial, &mut scratch)
        .context("reading handshake initiation")?;
    let remote_public = remote_static(&state)?;
    Ok(Responder { state, remote_public })
}

impl Responder {
    /// Produces the reply and the live session.
    pub fn finish(mut self, local_index: u32, remote_index: u32) -> Result<(Session, Vec<u8>)> {
        let mut buf = vec![0u8; HANDSHAKE_BUF];
        let len = self.state.write_message(&[], &mut buf)?;
        buf.truncate(len);
        if !self.state.is_handshake_finished() {
            bail!("handshake did not complete");
        }
        let session = Session {
            transport: self.state.into_stateless_transport_mode()?,
            local_index,
            remote_index,
            remote_public: self.remote_public,
            tx_counter: 0,
            replay: ReplayWindow::default(),
            established: Instant::now(),
        };
        Ok((session, buf))
    }
}

fn remote_static(state: &snow::HandshakeState) -> Result<[u8; 32]> {
    state
        .get_remote_static()
        .context("peer did not present a static key")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("peer static key was not 32 bytes"))
}

/// An established tunnel to one peer.
pub struct Session {
    transport: snow::StatelessTransportState,
    pub local_index: u32,
    pub remote_index: u32,
    pub remote_public: [u8; 32],
    tx_counter: u64,
    replay: ReplayWindow,
    pub established: Instant,
}

impl Session {
    /// Encrypts one packet, returning the counter to put in the header.
    pub fn encrypt(&mut self, plaintext: &[u8], out: &mut Vec<u8>) -> Result<u64> {
        let counter = self.tx_counter;
        self.tx_counter = self
            .tx_counter
            .checked_add(1)
            .context("nonce space exhausted; session must be rekeyed")?;
        out.resize(plaintext.len() + TAG_LEN, 0);
        let len = self.transport.write_message(counter, plaintext, out)?;
        out.truncate(len);
        Ok(counter)
    }

    /// Decrypts one packet.
    ///
    /// The replay window is only advanced *after* the tag verifies. Doing it the
    /// other way round would let an attacker who can spoof a datagram burn a
    /// counter far ahead of the real one and starve the session of legitimate
    /// packets.
    pub fn decrypt(&mut self, counter: u64, ciphertext: &[u8], out: &mut Vec<u8>) -> Result<usize> {
        if ciphertext.len() < TAG_LEN {
            bail!("ciphertext shorter than the authentication tag");
        }
        out.resize(ciphertext.len(), 0);
        let len = self
            .transport
            .read_message(counter, ciphertext, out)
            .map_err(|_| anyhow::anyhow!("packet failed authentication"))?;
        if !self.replay.accept(counter) {
            bail!("replayed or stale packet (counter {counter})");
        }
        out.truncate(len);
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair() -> ([u8; 32], [u8; 32]) {
        let private: [u8; 32] = rand::thread_rng().gen();
        let public = public_from_private(&private).unwrap();
        (private, public)
    }

    /// Drives a full handshake and hands back both sides' sessions.
    fn connected() -> (Session, Session) {
        let (alice_priv, _alice_pub) = keypair();
        let (bob_priv, bob_pub) = keypair();

        let (initiator, msg1) = initiate(&alice_priv, &bob_pub, 111).unwrap();
        let responder = respond(&bob_priv, &msg1).unwrap();
        let (bob_session, msg2) = responder.finish(222, 111).unwrap();
        let alice_session = initiator.finish(&msg2, 222).unwrap();
        (alice_session, bob_session)
    }

    #[test]
    fn public_key_derivation_matches_snow() {
        let keys = snow::Builder::new(NOISE_PARAMS.parse().unwrap())
            .generate_keypair()
            .unwrap();
        let private: [u8; 32] = keys.private.try_into().unwrap();
        let expected: [u8; 32] = keys.public.try_into().unwrap();
        assert_eq!(public_from_private(&private).unwrap(), expected);
    }

    #[test]
    fn handshake_agrees_on_identities_and_indexes() {
        let (alice_priv, alice_pub) = keypair();
        let (bob_priv, bob_pub) = keypair();

        let (initiator, msg1) = initiate(&alice_priv, &bob_pub, 111).unwrap();
        let responder = respond(&bob_priv, &msg1).unwrap();
        // Bob learns who is calling before he answers.
        assert_eq!(responder.remote_public, alice_pub);

        let (bob_session, msg2) = responder.finish(222, 111).unwrap();
        let alice_session = initiator.finish(&msg2, 222).unwrap();

        assert_eq!(alice_session.remote_public, bob_pub);
        assert_eq!(alice_session.local_index, 111);
        assert_eq!(alice_session.remote_index, 222);
        assert_eq!(bob_session.local_index, 222);
        assert_eq!(bob_session.remote_index, 111);
    }

    #[test]
    fn traffic_flows_in_both_directions() {
        let (mut alice, mut bob) = connected();
        let mut wire = Vec::new();
        let mut plain = Vec::new();

        let counter = alice.encrypt(b"hello bob", &mut wire).unwrap();
        let len = bob.decrypt(counter, &wire, &mut plain).unwrap();
        assert_eq!(&plain[..len], b"hello bob");

        let counter = bob.encrypt(b"hello alice", &mut wire).unwrap();
        let len = alice.decrypt(counter, &wire, &mut plain).unwrap();
        assert_eq!(&plain[..len], b"hello alice");
    }

    #[test]
    fn ciphertext_is_not_plaintext() {
        let (mut alice, _bob) = connected();
        let mut wire = Vec::new();
        alice.encrypt(b"secret payload", &mut wire).unwrap();
        assert!(!wire.windows(6).any(|w| w == b"secret"));
    }

    #[test]
    fn out_of_order_delivery_is_accepted() {
        let (mut alice, mut bob) = connected();
        let mut plain = Vec::new();
        let mut packets = Vec::new();
        for i in 0..8u8 {
            let mut wire = Vec::new();
            let counter = alice.encrypt(&[i], &mut wire).unwrap();
            packets.push((counter, wire));
        }
        // Deliver them backwards.
        for (counter, wire) in packets.iter().rev() {
            let len = bob.decrypt(*counter, wire, &mut plain).unwrap();
            assert_eq!(len, 1);
        }
    }

    #[test]
    fn replayed_packet_is_rejected() {
        let (mut alice, mut bob) = connected();
        let mut wire = Vec::new();
        let mut plain = Vec::new();
        let counter = alice.encrypt(b"once", &mut wire).unwrap();
        assert!(bob.decrypt(counter, &wire, &mut plain).is_ok());
        assert!(bob.decrypt(counter, &wire, &mut plain).is_err());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let (mut alice, mut bob) = connected();
        let mut wire = Vec::new();
        let mut plain = Vec::new();
        let counter = alice.encrypt(b"important", &mut wire).unwrap();
        wire[0] ^= 0x01;
        assert!(bob.decrypt(counter, &wire, &mut plain).is_err());
    }

    #[test]
    fn wrong_counter_is_rejected() {
        let (mut alice, mut bob) = connected();
        let mut wire = Vec::new();
        let mut plain = Vec::new();
        alice.encrypt(b"important", &mut wire).unwrap();
        assert!(bob.decrypt(9999, &wire, &mut plain).is_err());
    }

    /// A forged packet must not be able to advance the replay window and lock
    /// out the genuine traffic that follows it.
    #[test]
    fn forged_packet_does_not_starve_the_session() {
        let (mut alice, mut bob) = connected();
        let mut plain = Vec::new();
        let forged = vec![0u8; 32];
        assert!(bob.decrypt(u64::MAX - 1, &forged, &mut plain).is_err());

        let mut wire = Vec::new();
        let counter = alice.encrypt(b"genuine", &mut wire).unwrap();
        let len = bob.decrypt(counter, &wire, &mut plain).unwrap();
        assert_eq!(&plain[..len], b"genuine");
    }

    #[test]
    fn handshake_to_the_wrong_key_fails() {
        let (alice_priv, _) = keypair();
        let (_bob_priv, _bob_pub) = keypair();
        let (mallory_priv, _) = keypair();
        let (_, wrong_pub) = keypair();

        // Alice dials a key that nobody holds the private half of.
        let (_initiator, msg1) = initiate(&alice_priv, &wrong_pub, 1).unwrap();
        // Mallory cannot read a handshake addressed to someone else.
        assert!(respond(&mallory_priv, &msg1).is_err());
    }

    #[test]
    fn two_sessions_from_the_same_identity_do_not_share_keys() {
        let (mut alice_a, _) = connected();
        let (_, mut bob_b) = connected();
        let mut wire = Vec::new();
        let mut plain = Vec::new();
        let counter = alice_a.encrypt(b"cross talk", &mut wire).unwrap();
        assert!(bob_b.decrypt(counter, &wire, &mut plain).is_err());
    }
}
