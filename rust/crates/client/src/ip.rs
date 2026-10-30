//! Just enough IPv4 parsing to decide where a packet from the tun device goes.

use std::net::Ipv4Addr;

use hamradnet_proto::VIRTUAL_BROADCAST;

const MIN_HEADER_LEN: usize = 20;

fn is_ipv4(packet: &[u8]) -> bool {
    packet.len() >= MIN_HEADER_LEN && (packet[0] >> 4) == 4
}

pub fn destination(packet: &[u8]) -> Option<Ipv4Addr> {
    if !is_ipv4(packet) {
        return None;
    }
    Some(Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]))
}

pub fn source(packet: &[u8]) -> Option<Ipv4Addr> {
    if !is_ipv4(packet) {
        return None;
    }
    Some(Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]))
}

/// Should this destination be delivered to every peer rather than one?
///
/// This is the piece that makes "scan for LAN games" work. A game looking for
/// servers sends its discovery probe to the broadcast address; on a real LAN the
/// switch floods it to every port, and here we do the same by copying it to
/// every peer. Multicast is included because newer titles and mDNS/UPnP-based
/// discovery use it instead.
pub fn is_fanout(addr: Ipv4Addr) -> bool {
    addr == Ipv4Addr::BROADCAST
        || addr == Ipv4Addr::from(VIRTUAL_BROADCAST)
        || addr.is_multicast()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal well-formed IPv4 header with the given source and destination.
    fn packet(src: [u8; 4], dst: [u8; 4]) -> Vec<u8> {
        let mut buf = vec![0u8; MIN_HEADER_LEN];
        buf[0] = 0x45; // version 4, 5 32-bit words of header
        buf[9] = 17; // UDP
        buf[12..16].copy_from_slice(&src);
        buf[16..20].copy_from_slice(&dst);
        buf
    }

    #[test]
    fn reads_source_and_destination() {
        let p = packet([10, 66, 0, 2], [10, 66, 0, 7]);
        assert_eq!(source(&p), Some(Ipv4Addr::new(10, 66, 0, 2)));
        assert_eq!(destination(&p), Some(Ipv4Addr::new(10, 66, 0, 7)));
    }

    #[test]
    fn rejects_truncated_and_non_ipv4() {
        assert_eq!(destination(&[]), None);
        assert_eq!(destination(&[0x45; 19]), None);
        let mut ipv6 = vec![0u8; 40];
        ipv6[0] = 0x60;
        assert_eq!(destination(&ipv6), None, "IPv6 is not routed");
    }

    #[test]
    fn recognises_addresses_that_must_reach_everyone() {
        assert!(is_fanout(Ipv4Addr::new(255, 255, 255, 255)));
        assert!(is_fanout(Ipv4Addr::new(10, 66, 255, 255)));
        assert!(is_fanout(Ipv4Addr::new(224, 0, 0, 251)), "mDNS");
        assert!(is_fanout(Ipv4Addr::new(239, 255, 255, 250)), "SSDP/UPnP");
    }

    #[test]
    fn ordinary_peer_addresses_are_not_fanout() {
        assert!(!is_fanout(Ipv4Addr::new(10, 66, 0, 7)));
        assert!(!is_fanout(Ipv4Addr::new(10, 66, 1, 1)));
    }
}
