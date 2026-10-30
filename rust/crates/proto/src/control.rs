//! Control plane: length-prefixed JSON frames over TCP.
//!
//! Frame layout: `[len: u32 big-endian][json bytes]`. JSON is used rather than a
//! compact binary encoding because this plane is low-volume (a handful of frames
//! per peer per minute) and being able to read it with `tcpdump` is worth more
//! than the bytes saved.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

use crate::{PeerId, PublicKey};

/// Maximum size of a single control frame. Generous enough for a roster of a few
/// thousand peers, small enough that a malicious client cannot exhaust memory.
pub const MAX_FRAME_SIZE: u32 = 4 * 1024 * 1024;

/// A base64-encoded 32-byte key, so the JSON stays human-readable.
pub mod b64key {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(key: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        STANDARD.encode(key).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(d)?;
        let bytes = STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)?;
        bytes
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected a 32-byte key"))
    }
}

/// What a client knows about one of its peers. Everything needed to open a
/// tunnel: who they are, the key to encrypt to, and where to try reaching them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerInfo {
    pub peer_id: PeerId,
    /// Display name the peer chose. Untrusted - never used for addressing.
    pub name: String,
    #[serde(with = "b64key")]
    pub public_key: PublicKey,
    /// Virtual address inside the tunnel, e.g. 10.66.0.7.
    pub virtual_ip: std::net::Ipv4Addr,
    /// Candidate underlay addresses, best-guess first: LAN addresses the peer
    /// reported plus the public address the server observed for it.
    pub endpoints: Vec<SocketAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkInfo {
    pub network_id: String,
    pub name: String,
    /// This client's address within the network.
    pub virtual_ip: std::net::Ipv4Addr,
    pub prefix_len: u8,
}

/// Client -> server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Always the first frame on a fresh connection.
    Hello {
        version: u32,
        #[serde(with = "b64key")]
        public_key: PublicKey,
        name: String,
    },
    CreateNetwork {
        name: String,
        password: String,
    },
    JoinNetwork {
        network_id: String,
        password: String,
    },
    LeaveNetwork {
        network_id: String,
    },
    /// Removes a network for everyone. Only the peer whose key created it may
    /// do this.
    DeleteNetwork {
        network_id: String,
    },
    /// Locally-discovered candidate addresses (LAN interfaces, and whatever the
    /// reflection probe learned). The server appends the address it observes and
    /// forwards the set to the client's peers.
    UpdateEndpoints {
        endpoints: Vec<SocketAddr>,
    },
    Ping,
}

/// Server -> client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Answer to `Hello`. Carries everything needed to talk to the UDP side.
    Welcome {
        version: u32,
        peer_id: PeerId,
        /// Address assigned for the lifetime of this connection. Sent here, and
        /// not only on join, so the client can bring its interface up straight
        /// away rather than waiting to be let into a network.
        virtual_ip: std::net::Ipv4Addr,
        prefix_len: u8,
        /// UDP port on this server for relay and reflection.
        udp_port: u16,
        /// Proves to the UDP side that this peer_id belongs to us.
        #[serde(with = "hex_token")]
        relay_token: [u8; crate::wire::RELAY_TOKEN_LEN],
    },
    NetworkCreated {
        network_id: String,
        name: String,
    },
    /// Sent on a successful join, and again after a reconnect.
    Joined {
        network: NetworkInfo,
        peers: Vec<PeerInfo>,
    },
    Left {
        network_id: String,
    },
    /// The network is gone. Sent to every member, including the one who asked.
    Deleted {
        network_id: String,
    },
    /// A peer joined, or its endpoints changed. Replaces any prior entry.
    PeerUpdate {
        network_id: String,
        peer: PeerInfo,
    },
    PeerLeft {
        network_id: String,
        peer_id: PeerId,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
    Pong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    VersionMismatch,
    NoSuchNetwork,
    BadPassword,
    AlreadyJoined,
    NetworkFull,
    NotOwner,
    RateLimited,
    Internal,
}

mod hex_token {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    const N: usize = crate::wire::RELAY_TOKEN_LEN;

    pub fn serialize<S: Serializer>(token: &[u8; N], s: S) -> Result<S::Ok, S::Error> {
        let mut text = String::with_capacity(N * 2);
        for byte in token {
            text.push_str(&format!("{byte:02x}"));
        }
        text.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; N], D::Error> {
        let text = String::deserialize(d)?;
        if text.len() != N * 2 {
            return Err(serde::de::Error::custom("bad relay token length"));
        }
        let mut out = [0u8; N];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(out)
    }
}

/// Generates a network ID in the shape `HAM-4821-9903` - short enough to read
/// aloud over voice chat, large enough (10^8) that guessing one is impractical
/// when it is also password-protected.
pub fn generate_network_id(rng: &mut impl rand::Rng) -> String {
    format!("HAM-{:04}-{:04}", rng.gen_range(0..10_000), rng.gen_range(0..10_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips() {
        let msg = ClientMsg::Hello {
            version: crate::PROTOCOL_VERSION,
            public_key: [7u8; 32],
            name: "konako".into(),
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert!(text.contains("\"t\":\"hello\""));
        match serde_json::from_str::<ClientMsg>(&text).unwrap() {
            ClientMsg::Hello { public_key, name, .. } => {
                assert_eq!(public_key, [7u8; 32]);
                assert_eq!(name, "konako");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn welcome_round_trips_relay_token() {
        let msg = ServerMsg::Welcome {
            version: 1,
            peer_id: 42,
            virtual_ip: std::net::Ipv4Addr::new(10, 66, 0, 2),
            prefix_len: 16,
            udp_port: 7778,
            relay_token: [0xab; crate::wire::RELAY_TOKEN_LEN],
        };
        let text = serde_json::to_string(&msg).unwrap();
        match serde_json::from_str::<ServerMsg>(&text).unwrap() {
            ServerMsg::Welcome { relay_token, peer_id, .. } => {
                assert_eq!(relay_token, [0xab; crate::wire::RELAY_TOKEN_LEN]);
                assert_eq!(peer_id, 42);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn network_id_has_expected_shape() {
        let mut rng = rand::thread_rng();
        let id = generate_network_id(&mut rng);
        assert_eq!(id.len(), 13);
        assert!(id.starts_with("HAM-"));
    }
}
