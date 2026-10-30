//! Wire protocol shared by the HamRadNet coordinator and clients.
//!
//! Two planes:
//!   * control plane - length-prefixed JSON over TCP, client <-> server only.
//!   * data plane - binary UDP packets, client <-> client (direct), or
//!     client <-> server <-> client when relayed.

pub mod control;
pub mod wire;

pub use control::{ClientMsg, ErrorCode, NetworkInfo, PeerInfo, ServerMsg};
pub use wire::{Packet, PacketType, RELAY_TOKEN_LEN};

/// Bumped whenever the control or data plane changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default TCP port for the control plane.
pub const DEFAULT_CONTROL_PORT: u16 = 7777;
/// Default UDP port for relay + endpoint reflection.
pub const DEFAULT_UDP_PORT: u16 = 7778;

/// Virtual subnet handed out to peers: 10.66.0.0/16.
pub const VIRTUAL_SUBNET: [u8; 4] = [10, 66, 0, 0];
pub const VIRTUAL_PREFIX_LEN: u8 = 16;
pub const VIRTUAL_BROADCAST: [u8; 4] = [10, 66, 255, 255];

/// MTU of the virtual interface.
///
/// 1280 is the IPv6 minimum every path must carry, so a packet this size gets
/// through anything. It was briefly raised to 1380 to squeeze out more payload
/// per packet; that was a mistake. Two clients on different MTUs meant one side
/// sending packets the other's interface refused, and the throughput gained was
/// not worth a setting that has to match everywhere to work.
pub const TUN_MTU: u32 = 1280;

/// Payloads inside the tunnel are normally IP packets, whose first byte carries
/// the version in the high nibble and is therefore always >= 0x40. A leading
/// zero byte marks one of our own in-tunnel control messages instead, which is
/// how latency is measured without a separate unencrypted channel.
pub const CONTROL_PREFIX: u8 = 0x00;
pub const CONTROL_PING: u8 = 0x01;
pub const CONTROL_PONG: u8 = 0x02;

/// Largest UDP payload we will ever send or expect.
pub const MAX_PACKET_SIZE: usize = 1500;

/// Identifier assigned to a connected client by the server, unique per server.
pub type PeerId = u32;

/// A 32-byte X25519 public key, serialised as base64 in the control plane.
pub type PublicKey = [u8; 32];

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("packet too short: {0} bytes")]
    TooShort(usize),
    #[error("unknown packet type: 0x{0:02x}")]
    UnknownType(u8),
    #[error("packet exceeds maximum size")]
    TooLong,
    #[error("malformed control frame: {0}")]
    Json(#[from] serde_json::Error),
}
