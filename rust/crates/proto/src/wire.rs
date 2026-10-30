//! Data plane framing: binary packets over UDP.
//!
//! Every packet starts with a one-byte type tag. Session packets (`0x01`-`0x03`)
//! are end-to-end between two clients and are byte-identical whether they travel
//! directly or wrapped in a relay header, so the session layer never has to care
//! which path carried them.
//!
//! ```text
//!   0x01 HandshakeInit   [tag][sender_index u32][noise message]
//!   0x02 HandshakeResp   [tag][sender_index u32][receiver_index u32][noise message]
//!   0x03 Data            [tag][receiver_index u32][counter u64][ciphertext]
//!   0x04 Punch           [tag][sender_peer_id u32]                client -> client
//!   0x10 RelayRegister   [tag][peer_id u32][token 16]            client -> server
//!   0x11 RelayForward    [tag][dst_peer_id u32][inner packet]    client -> server
//!   0x12 RelayDeliver    [tag][src_peer_id u32][inner packet]    server -> client
//!   0x20 ReflectRequest  [tag][txid u64]                         client -> server
//!   0x21 ReflectResponse [tag][txid u64][addr]                   server -> client
//! ```

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::{PeerId, ProtoError};

/// Length of the token that authorises a peer to use the relay.
pub const RELAY_TOKEN_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    HandshakeInit = 0x01,
    HandshakeResp = 0x02,
    Data = 0x03,
    Punch = 0x04,
    RelayRegister = 0x10,
    RelayForward = 0x11,
    RelayDeliver = 0x12,
    ReflectRequest = 0x20,
    ReflectResponse = 0x21,
}

impl PacketType {
    pub fn from_u8(byte: u8) -> Result<Self, ProtoError> {
        Ok(match byte {
            0x01 => Self::HandshakeInit,
            0x02 => Self::HandshakeResp,
            0x03 => Self::Data,
            0x04 => Self::Punch,
            0x10 => Self::RelayRegister,
            0x11 => Self::RelayForward,
            0x12 => Self::RelayDeliver,
            0x20 => Self::ReflectRequest,
            0x21 => Self::ReflectResponse,
            other => return Err(ProtoError::UnknownType(other)),
        })
    }
}

/// A decoded packet. Payloads borrow from the receive buffer so that forwarding
/// a relayed packet does not copy it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet<'a> {
    HandshakeInit {
        sender_index: u32,
        noise: &'a [u8],
    },
    HandshakeResp {
        sender_index: u32,
        receiver_index: u32,
        noise: &'a [u8],
    },
    Data {
        receiver_index: u32,
        counter: u64,
        ciphertext: &'a [u8],
    },
    /// Sent to a peer's candidate addresses purely to open an outbound NAT
    /// mapping, so the peer's handshake can get back in. Carries nothing worth
    /// authenticating and is never answered, so it cannot be used to amplify.
    Punch {
        sender: PeerId,
    },
    RelayRegister {
        peer_id: PeerId,
        token: [u8; RELAY_TOKEN_LEN],
    },
    RelayForward {
        dst: PeerId,
        inner: &'a [u8],
    },
    RelayDeliver {
        src: PeerId,
        inner: &'a [u8],
    },
    ReflectRequest {
        txid: u64,
    },
    ReflectResponse {
        txid: u64,
        observed: SocketAddr,
    },
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn u64_at(buf: &[u8], off: usize) -> u64 {
    let mut out = [0u8; 8];
    out.copy_from_slice(&buf[off..off + 8]);
    u64::from_be_bytes(out)
}

fn need(buf: &[u8], n: usize) -> Result<(), ProtoError> {
    if buf.len() < n {
        Err(ProtoError::TooShort(buf.len()))
    } else {
        Ok(())
    }
}

impl<'a> Packet<'a> {
    pub fn decode(buf: &'a [u8]) -> Result<Self, ProtoError> {
        need(buf, 1)?;
        Ok(match PacketType::from_u8(buf[0])? {
            PacketType::HandshakeInit => {
                need(buf, 5)?;
                Packet::HandshakeInit {
                    sender_index: u32_at(buf, 1),
                    noise: &buf[5..],
                }
            }
            PacketType::HandshakeResp => {
                need(buf, 9)?;
                Packet::HandshakeResp {
                    sender_index: u32_at(buf, 1),
                    receiver_index: u32_at(buf, 5),
                    noise: &buf[9..],
                }
            }
            PacketType::Data => {
                need(buf, 13)?;
                Packet::Data {
                    receiver_index: u32_at(buf, 1),
                    counter: u64_at(buf, 5),
                    ciphertext: &buf[13..],
                }
            }
            PacketType::Punch => {
                need(buf, 5)?;
                Packet::Punch { sender: u32_at(buf, 1) }
            }
            PacketType::RelayRegister => {
                need(buf, 5 + RELAY_TOKEN_LEN)?;
                let mut token = [0u8; RELAY_TOKEN_LEN];
                token.copy_from_slice(&buf[5..5 + RELAY_TOKEN_LEN]);
                Packet::RelayRegister {
                    peer_id: u32_at(buf, 1),
                    token,
                }
            }
            PacketType::RelayForward => {
                need(buf, 5)?;
                Packet::RelayForward {
                    dst: u32_at(buf, 1),
                    inner: &buf[5..],
                }
            }
            PacketType::RelayDeliver => {
                need(buf, 5)?;
                Packet::RelayDeliver {
                    src: u32_at(buf, 1),
                    inner: &buf[5..],
                }
            }
            PacketType::ReflectRequest => {
                need(buf, 9)?;
                Packet::ReflectRequest {
                    txid: u64_at(buf, 1),
                }
            }
            PacketType::ReflectResponse => {
                need(buf, 9)?;
                Packet::ReflectResponse {
                    txid: u64_at(buf, 1),
                    observed: decode_addr(&buf[9..])?,
                }
            }
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        match *self {
            Packet::HandshakeInit { sender_index, noise } => {
                out.push(PacketType::HandshakeInit as u8);
                out.extend_from_slice(&sender_index.to_be_bytes());
                out.extend_from_slice(noise);
            }
            Packet::HandshakeResp { sender_index, receiver_index, noise } => {
                out.push(PacketType::HandshakeResp as u8);
                out.extend_from_slice(&sender_index.to_be_bytes());
                out.extend_from_slice(&receiver_index.to_be_bytes());
                out.extend_from_slice(noise);
            }
            Packet::Data { receiver_index, counter, ciphertext } => {
                out.push(PacketType::Data as u8);
                out.extend_from_slice(&receiver_index.to_be_bytes());
                out.extend_from_slice(&counter.to_be_bytes());
                out.extend_from_slice(ciphertext);
            }
            Packet::Punch { sender } => {
                out.push(PacketType::Punch as u8);
                out.extend_from_slice(&sender.to_be_bytes());
            }
            Packet::RelayRegister { peer_id, token } => {
                out.push(PacketType::RelayRegister as u8);
                out.extend_from_slice(&peer_id.to_be_bytes());
                out.extend_from_slice(&token);
            }
            Packet::RelayForward { dst, inner } => {
                out.push(PacketType::RelayForward as u8);
                out.extend_from_slice(&dst.to_be_bytes());
                out.extend_from_slice(inner);
            }
            Packet::RelayDeliver { src, inner } => {
                out.push(PacketType::RelayDeliver as u8);
                out.extend_from_slice(&src.to_be_bytes());
                out.extend_from_slice(inner);
            }
            Packet::ReflectRequest { txid } => {
                out.push(PacketType::ReflectRequest as u8);
                out.extend_from_slice(&txid.to_be_bytes());
            }
            Packet::ReflectResponse { txid, observed } => {
                out.push(PacketType::ReflectResponse as u8);
                out.extend_from_slice(&txid.to_be_bytes());
                encode_addr(&observed, out);
            }
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        self.encode(&mut out);
        out
    }
}

/// `[family: 4|6][addr bytes][port u16]`
pub fn encode_addr(addr: &SocketAddr, out: &mut Vec<u8>) {
    match addr.ip() {
        IpAddr::V4(v4) => {
            out.push(4);
            out.extend_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            out.push(6);
            out.extend_from_slice(&v6.octets());
        }
    }
    out.extend_from_slice(&addr.port().to_be_bytes());
}

pub fn decode_addr(buf: &[u8]) -> Result<SocketAddr, ProtoError> {
    need(buf, 1)?;
    match buf[0] {
        4 => {
            need(buf, 7)?;
            let ip = Ipv4Addr::new(buf[1], buf[2], buf[3], buf[4]);
            let port = u16::from_be_bytes([buf[5], buf[6]]);
            Ok(SocketAddr::new(IpAddr::V4(ip), port))
        }
        6 => {
            need(buf, 19)?;
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&buf[1..17]);
            let port = u16::from_be_bytes([buf[17], buf[18]]);
            Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        other => Err(ProtoError::UnknownType(other)),
    }
}

/// Replay protection over a sliding window, the same scheme WireGuard uses.
/// Counters arriving out of order within the window are accepted once; anything
/// older than the window, or a repeat, is rejected.
#[derive(Debug, Default)]
pub struct ReplayWindow {
    highest: u64,
    bitmap: u64,
}

impl ReplayWindow {
    pub const WINDOW: u64 = 64;

    /// Returns true if `counter` is fresh, and records it. Returns false for a
    /// replay or a counter too old to verify.
    pub fn accept(&mut self, counter: u64) -> bool {
        if counter > self.highest {
            let shift = counter - self.highest;
            self.bitmap = if shift >= Self::WINDOW {
                0
            } else {
                self.bitmap << shift
            };
            self.bitmap |= 1;
            self.highest = counter;
            true
        } else {
            let behind = self.highest - counter;
            if behind >= Self::WINDOW {
                return false;
            }
            let mask = 1u64 << behind;
            if self.bitmap & mask != 0 {
                false
            } else {
                self.bitmap |= mask;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(packet: Packet<'_>) {
        let bytes = packet.to_vec();
        assert_eq!(Packet::decode(&bytes).unwrap(), packet);
    }

    #[test]
    fn packets_round_trip() {
        round_trip(Packet::HandshakeInit { sender_index: 9, noise: &[1, 2, 3] });
        round_trip(Packet::HandshakeResp {
            sender_index: 9,
            receiver_index: 4,
            noise: &[5, 6],
        });
        round_trip(Packet::Data {
            receiver_index: 3,
            counter: u64::MAX,
            ciphertext: &[9; 40],
        });
        round_trip(Packet::Punch { sender: 5 });
        round_trip(Packet::RelayForward { dst: 77, inner: &[0x03, 0, 0] });
        round_trip(Packet::RelayDeliver { src: 77, inner: &[0x03, 0, 0] });
        round_trip(Packet::ReflectRequest { txid: 12345 });
    }

    #[test]
    fn reflect_response_round_trips_both_families() {
        round_trip(Packet::ReflectResponse {
            txid: 1,
            observed: "203.0.113.9:41234".parse().unwrap(),
        });
        round_trip(Packet::ReflectResponse {
            txid: 2,
            observed: "[2001:db8::1]:41234".parse().unwrap(),
        });
    }

    #[test]
    fn truncated_packets_are_rejected_not_panicking() {
        for len in 0..13 {
            let buf = vec![0x03u8; len];
            assert!(Packet::decode(&buf).is_err(), "len {len} should be rejected");
        }
    }

    #[test]
    fn unknown_type_is_rejected() {
        assert!(Packet::decode(&[0xff, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn replay_window_accepts_in_order() {
        let mut window = ReplayWindow::default();
        for counter in 1..1000 {
            assert!(window.accept(counter), "counter {counter} should be fresh");
        }
    }

    #[test]
    fn replay_window_rejects_repeats() {
        let mut window = ReplayWindow::default();
        assert!(window.accept(5));
        assert!(!window.accept(5));
        assert!(window.accept(4));
        assert!(!window.accept(4));
    }

    #[test]
    fn replay_window_accepts_reordering_but_rejects_ancient() {
        let mut window = ReplayWindow::default();
        assert!(window.accept(100));
        assert!(window.accept(99));
        assert!(window.accept(60));
        // 100 - 64 = 36, so anything at or below that is outside the window.
        assert!(!window.accept(36));
        assert!(!window.accept(1));
    }
}
