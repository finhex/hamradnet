//! Copies games' LAN broadcasts into the tunnel.
//!
//! A broadcast to 255.255.255.255 leaves the computer through one interface
//! only, the one the default route uses: the real network card, never the
//! tunnel. A game that announces itself that way, as Warcraft III and many
//! others do, would therefore never be heard by the other players.
//!
//! So the daemon watches for such broadcasts going out of every other interface
//! and sends a copy through the tunnel too, from this machine's tunnel address
//! so that answers come back the same way. The original still reaches the local
//! network. On Linux that is [`spawn`]; on Windows, which offers no such view,
//! the tunnel is made the interface broadcasts prefer instead (see
//! `tun/windows.rs`).

use std::net::Ipv4Addr;

use hamradnet_proto::TUN_MTU;

/// Ports of services that announce this computer to its own network, not of
/// games, so they stay off the tunnel: DHCP, NetBIOS, KDE Connect, Syncthing
/// and Steam's streaming discovery. The Go client skips the same list.
pub const SKIPPED_PORTS: [u16; 7] = [67, 68, 137, 138, 1716, 21027, 27036];

/// Turns a broadcast seen leaving another interface into one for the tunnel:
/// the same packet, sent from `source`. `None` for anything that is not a whole
/// UDP broadcast worth copying.
pub fn rebroadcast(packet: &[u8], source: Ipv4Addr) -> Option<Vec<u8>> {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return None;
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    let total = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if header_len < 20 || total < header_len + 8 || total > packet.len() || total > TUN_MTU as usize {
        return None;
    }
    // Only the first fragment has a UDP header, so fragments are left alone.
    let fragmented = u16::from_be_bytes([packet[6], packet[7]]) & 0x3fff != 0;
    if fragmented || packet[9] != 17 || packet[16..20] != [255; 4] {
        return None;
    }
    let udp = &packet[header_len..total];
    let udp_len = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    let port = u16::from_be_bytes([udp[2], udp[3]]);
    if udp_len != udp.len() || SKIPPED_PORTS.contains(&port) {
        return None;
    }

    let mut out = packet[..total].to_vec();
    out[12..16].copy_from_slice(&source.octets());
    out[10..12].fill(0);
    let header_sum = checksum(&out[..header_len], 0);
    out[10..12].copy_from_slice(&header_sum.to_be_bytes());

    // The UDP checksum covers the addresses too. It is computed afresh rather
    // than adjusted: a packet caught on its way out may still hold the partial
    // sum the network card was going to finish.
    let mut pseudo = 17 + (total - header_len) as u32;
    for word in out[12..20].chunks(2) {
        pseudo += u32::from(u16::from_be_bytes([word[0], word[1]]));
    }
    let segment = &mut out[header_len..];
    segment[6..8].fill(0);
    let mut sum = checksum(segment, pseudo);
    if sum == 0 {
        // Zero means "no checksum" in UDP, so a real zero is sent as all ones.
        sum = 0xffff;
    }
    segment[6..8].copy_from_slice(&sum.to_be_bytes());
    Some(out)
}

/// The Internet checksum of `data`, folded in on top of `initial`.
fn checksum(data: &[u8], initial: u32) -> u16 {
    let mut sum = initial;
    let mut words = data.chunks_exact(2);
    for word in &mut words {
        sum += u32::from(u16::from_be_bytes([word[0], word[1]]));
    }
    if let [last] = words.remainder() {
        sum += u32::from(*last) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(target_os = "linux")]
pub use capture::spawn;

#[cfg(target_os = "linux")]
mod capture {
    use std::ffi::CString;
    use std::io;
    use std::net::Ipv4Addr;

    use anyhow::{bail, Context, Result};

    /// `SKF_AD_OFF + SKF_AD_PKTTYPE`: loads the packet's direction, not its data.
    const LOAD_PACKET_TYPE: u32 = 0xffff_f004;
    /// `SKF_AD_OFF + SKF_AD_PROTOCOL`: loads the packet's link-layer protocol.
    const LOAD_PROTOCOL: u32 = 0xffff_f000;
    const PACKET_OUTGOING: u8 = 4;

    /// Runs in the kernel, so the thread is only woken for what it wants:
    /// IPv4 packets on their way out, UDP, addressed to 255.255.255.255. The
    /// packet socket hands it each packet from the IP header on.
    fn filter() -> [libc::sock_filter; 10] {
        let op = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
        [
            op(0x20, 0, 0, LOAD_PACKET_TYPE),         // A = packet type
            op(0x15, 0, 7, PACKET_OUTGOING.into()),   // not outgoing: drop
            op(0x20, 0, 0, LOAD_PROTOCOL),            // A = protocol
            op(0x15, 0, 5, libc::ETH_P_IP as u32),    // not IPv4: drop
            op(0x30, 0, 0, 9),                        // A = IP protocol
            op(0x15, 0, 3, libc::IPPROTO_UDP as u32), // not UDP: drop
            op(0x20, 0, 0, 16),                       // A = destination address
            op(0x15, 0, 1, 0xffff_ffff),              // not 255.255.255.255: drop
            op(0x06, 0, 0, 0xffff),                   // keep
            op(0x06, 0, 0, 0),                        // drop
        ]
    }

    /// Starts copying games' broadcasts into the tunnel named `tun_name`.
    /// `address` gives the current tunnel address, and `deliver` hands a copy to
    /// the router, returning false once the router is gone.
    pub fn spawn(
        tun_name: &str,
        address: impl Fn() -> Option<Ipv4Addr> + Send + 'static,
        deliver: impl Fn(Vec<u8>) -> bool + Send + 'static,
    ) -> Result<()> {
        let name = CString::new(tun_name)?;
        let tun_index = unsafe { libc::if_nametoindex(name.as_ptr()) } as libc::c_int;
        if tun_index == 0 {
            bail!("there is no interface named {tun_name}");
        }

        // Listening for every protocol is not a choice: only such sockets are
        // shown outgoing packets. The filter keeps the IPv4 ones.
        let protocol = (libc::ETH_P_ALL as u16).to_be() as libc::c_int;
        let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, protocol) };
        if fd < 0 {
            return Err(io::Error::last_os_error()).context("opening a packet socket");
        }
        let program = filter();
        let fprog = libc::sock_fprog { len: program.len() as u16, filter: program.as_ptr().cast_mut() };
        let attached = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ATTACH_FILTER,
                std::ptr::from_ref(&fprog).cast(),
                std::mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
            )
        };
        if attached < 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err).context("attaching the broadcast filter");
        }

        std::thread::Builder::new()
            .name("broadcast-copy".into())
            .spawn(move || {
                let mut buf = vec![0u8; 1 << 16];
                loop {
                    let mut from: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
                    let mut from_len = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
                    let len = unsafe {
                        libc::recvfrom(
                            fd,
                            buf.as_mut_ptr().cast(),
                            buf.len(),
                            0,
                            std::ptr::from_mut(&mut from).cast(),
                            &mut from_len,
                        )
                    };
                    if len < 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        tracing::warn!(%err, "stopped copying LAN broadcasts into the tunnel");
                        break;
                    }
                    // What the tunnel itself sends out is already where it should be.
                    if from.sll_pkttype != PACKET_OUTGOING || from.sll_ifindex == tun_index {
                        continue;
                    }
                    let Some(source) = address() else { continue };
                    if let Some(packet) = super::rebroadcast(&buf[..len as usize], source) {
                        if !deliver(packet) {
                            break;
                        }
                    }
                }
                unsafe { libc::close(fd) };
            })
            .context("spawning the broadcast thread")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: [u8; 4] = [192, 168, 1, 5];
    const TUNNEL: Ipv4Addr = Ipv4Addr::new(10, 66, 0, 2);

    fn pseudo_sum(p: &[u8]) -> u32 {
        let mut sum = 17 + (p.len() - 20) as u32;
        for word in p[12..20].chunks(2) {
            sum += u32::from(u16::from_be_bytes([word[0], word[1]]));
        }
        sum
    }

    /// An IPv4 UDP packet with correct checksums.
    fn udp_packet(src: [u8; 4], dst: [u8; 4], src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
        let total = 28 + payload.len();
        let mut p = vec![0u8; total];
        p[0] = 0x45;
        p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
        p[8] = 64;
        p[9] = 17;
        p[12..16].copy_from_slice(&src);
        p[16..20].copy_from_slice(&dst);
        let header = checksum(&p[..20], 0);
        p[10..12].copy_from_slice(&header.to_be_bytes());
        p[20..22].copy_from_slice(&src_port.to_be_bytes());
        p[22..24].copy_from_slice(&dst_port.to_be_bytes());
        p[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        p[28..].copy_from_slice(payload);
        let udp = checksum(&p[20..], pseudo_sum(&p));
        p[26..28].copy_from_slice(&udp.to_be_bytes());
        p
    }

    #[test]
    fn a_game_broadcast_is_resent_from_the_tunnel_address_with_valid_checksums() {
        let payload = b"\xf7\x2f\x10\x00PX3W\x1c\x00\x00\x00\x00\x00\x00\x00"; // a WC3 search
        let original = udp_packet(LAN, [255; 4], 6112, 6112, payload);
        let out = rebroadcast(&original, TUNNEL).expect("a game's broadcast was not copied");
        assert_eq!(out[12..16], TUNNEL.octets());
        assert_eq!(checksum(&out[..20], 0), 0, "IP header checksum");
        assert_eq!(checksum(&out[20..], pseudo_sum(&out)), 0, "UDP checksum");
        assert_eq!(&out[28..], payload);
        assert_eq!(original[12..16], LAN, "the original must not change");
    }

    /// A packet caught on its way out may carry only the partial sum the
    /// network card was meant to complete; the copy must still be valid.
    #[test]
    fn an_unfinished_checksum_is_repaired() {
        let mut p = udp_packet(LAN, [255; 4], 6112, 6112, b"hello");
        p[26..28].copy_from_slice(&0x1234u16.to_be_bytes());
        let out = rebroadcast(&p, TUNNEL).unwrap();
        assert_eq!(checksum(&out[20..], pseudo_sum(&out)), 0);
    }

    #[test]
    fn everything_else_is_left_alone() {
        let game = udp_packet(LAN, [255; 4], 6112, 6112, b"x");
        let mut fragment = game.clone();
        fragment[6] = 0x20;
        let mut tcp = game.clone();
        tcp[9] = 6;
        let cases: [(&str, Vec<u8>); 10] = [
            ("unicast", udp_packet(LAN, [192, 168, 1, 9], 6112, 6112, b"x")),
            ("subnet", udp_packet(LAN, [192, 168, 1, 255], 6112, 6112, b"x")),
            ("KDE Connect", udp_packet(LAN, [255; 4], 1716, 1716, b"x")),
            ("Syncthing", udp_packet(LAN, [255; 4], 21027, 21027, b"x")),
            ("DHCP", udp_packet([0; 4], [255; 4], 68, 67, b"x")),
            ("too big", udp_packet(LAN, [255; 4], 6112, 6112, &[0; 1300])),
            ("fragment", fragment),
            ("TCP", tcp),
            ("truncated", game[..game.len() - 1].to_vec()),
            ("not even IPv4", vec![0x60, 0, 0, 0]),
        ];
        for (name, packet) in cases {
            assert!(rebroadcast(&packet, TUNNEL).is_none(), "{name} was copied into the tunnel");
        }
    }
}
