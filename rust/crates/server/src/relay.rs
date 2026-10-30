//! UDP plane: endpoint reflection and relay fallback.
//!
//! The server never sees plaintext here. Relayed payloads are Noise transport
//! packets between two clients; the relay only rewrites the outer header and
//! forwards. Its job is to keep a game working for the pair of players whose
//! NATs refuse to hole-punch.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use hamradnet_proto::{wire::Packet, MAX_PACKET_SIZE};
use tokio::net::UdpSocket;

use crate::state::State;

pub async fn serve(socket: UdpSocket, state: Arc<Mutex<State>>) -> Result<()> {
    let mut buf = vec![0u8; MAX_PACKET_SIZE];
    let mut out = Vec::with_capacity(MAX_PACKET_SIZE);

    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(%err, "udp recv failed");
                continue;
            }
        };

        let packet = match Packet::decode(&buf[..len]) {
            Ok(packet) => packet,
            // Unsolicited internet noise on an open UDP port is routine.
            Err(err) => {
                tracing::trace!(%src, %err, "dropping undecodable datagram");
                continue;
            }
        };

        match packet {
            // Lets a client discover its own public address, which is the
            // candidate its peers will actually be able to reach.
            Packet::ReflectRequest { txid } => {
                Packet::ReflectResponse { txid, observed: src }.encode(&mut out);
                let _ = socket.send_to(&out, src).await;
            }

            Packet::RelayRegister { peer_id, token } => {
                let accepted = state.lock().unwrap().register_relay(peer_id, src, &token);
                if accepted {
                    // The address we just saw is also the best endpoint to
                    // publish to this peer's networks.
                    state.lock().unwrap().update_endpoints(peer_id, None, Some(src));
                    tracing::debug!(peer_id, %src, "relay registered");
                }
            }

            Packet::RelayForward { dst, inner } => {
                let target = {
                    let state = state.lock().unwrap();
                    let Some(sender) = state.peer_at(&src) else {
                        tracing::trace!(%src, "relay from an unregistered address");
                        continue;
                    };
                    if !state.shares_network(sender, dst) {
                        tracing::debug!(sender, dst, "relay refused: no shared network");
                        continue;
                    }
                    state.relay_addrs.get(&dst).copied().map(|addr| (sender, addr))
                };
                let Some((sender, addr)) = target else { continue };
                Packet::RelayDeliver { src: sender, inner }.encode(&mut out);
                let _ = socket.send_to(&out, addr).await;
            }

            // Everything else is client-to-client and has no business here.
            other => tracing::trace!(%src, ?other, "ignoring packet on the server plane"),
        }
    }
}
