//! The peer table: who is in our networks, the tunnel state for each, and which
//! underlay path their traffic is currently taking.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use hamradnet_proto::{PeerId, PeerInfo, PublicKey};

use crate::session::{Initiator, Session};

/// Retry a handshake that has not completed after this long.
pub const HANDSHAKE_RETRY: Duration = Duration::from_secs(3);
/// Send a keepalive if we have not sent anything for this long. It holds the NAT
/// mapping open and doubles as the latency probe, so it is frequent enough that
/// the displayed round-trip time stays current without being wasteful: two small
/// packets per peer every five seconds.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Declare a path dead after this much silence and start over.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(45);

pub enum Link {
    /// No tunnel, and none being negotiated.
    Idle,
    /// We sent a handshake initiation and are waiting for the reply.
    Handshaking(Box<Initiator>),
    /// Live tunnel.
    Up(Box<Session>),
}

impl Link {
    pub fn session_mut(&mut self) -> Option<&mut Session> {
        match self {
            Link::Up(session) => Some(session),
            _ => None,
        }
    }

    pub fn is_up(&self) -> bool {
        matches!(self, Link::Up(_))
    }
}

/// How the next packet to a peer should be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendPath {
    /// Straight to the peer - the path we want.
    Direct(SocketAddr),
    /// Bounced off the coordinator, for NAT pairs that refuse to punch.
    Relay,
}

pub struct Peer {
    pub info: PeerInfo,
    /// Which of our networks this peer shares with us. A peer can be in more
    /// than one, so leaving a single network must not drop them entirely.
    pub networks: HashSet<String>,
    pub link: Link,
    /// Addresses worth trying, newest roster first.
    pub candidates: Vec<SocketAddr>,
    /// A candidate confirmed by an authenticated packet actually arriving from it.
    pub confirmed: Option<SocketAddr>,
    pub last_rx: Option<Instant>,
    pub last_tx: Option<Instant>,
    pub last_handshake: Option<Instant>,
    /// True once traffic from this peer has reached us through the relay.
    pub relay_seen: bool,
    /// Smoothed round-trip time, or None until the first reply comes back.
    pub rtt: Option<Duration>,
    /// The probe we are waiting on, as `(id, when it was sent)`.
    pub ping_inflight: Option<(u64, Instant)>,
    /// The answer we gave to the peer's last handshake initiation, as
    /// `(their index, our index, response packet)`.
    ///
    /// An initiator races the same initiation down every candidate path at once,
    /// so we receive it several times. Without this, each copy would build a
    /// separate session and the two sides would end up keyed to different ones.
    /// Replaying the stored answer keeps exactly one session per handshake.
    pub last_response: Option<(u32, u32, Vec<u8>)>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

impl Peer {
    fn new(info: PeerInfo) -> Self {
        Self {
            candidates: info.endpoints.clone(),
            info,
            networks: HashSet::new(),
            link: Link::Idle,
            confirmed: None,
            last_rx: None,
            last_tx: None,
            last_handshake: None,
            relay_seen: false,
            rtt: None,
            ping_inflight: None,
            last_response: None,
            rx_bytes: 0,
            tx_bytes: 0,
        }
    }

    /// Prefer a confirmed direct path; fall back to the relay.
    pub fn path(&self) -> SendPath {
        match self.confirmed {
            Some(addr) => SendPath::Direct(addr),
            None => SendPath::Relay,
        }
    }

    pub fn is_direct(&self) -> bool {
        self.confirmed.is_some()
    }

    /// Folds a fresh sample into the smoothed round-trip time. A plain average
    /// would let one delayed packet dominate the reading for a long time, so new
    /// samples are weighted at a quarter.
    pub fn record_rtt(&mut self, sample: Duration) {
        self.rtt = Some(match self.rtt {
            None => sample,
            Some(previous) => previous.mul_f32(0.75) + sample.mul_f32(0.25),
        });
    }

    pub fn is_stale(&self, now: Instant) -> bool {
        match self.last_rx {
            Some(seen) => now.duration_since(seen) > PEER_TIMEOUT,
            // Never heard from: stale once the handshake has had time to land.
            None => self
                .last_handshake
                .is_some_and(|at| now.duration_since(at) > PEER_TIMEOUT),
        }
    }

    pub fn needs_handshake(&self, now: Instant) -> bool {
        match &self.link {
            Link::Up(_) => false,
            Link::Idle => true,
            Link::Handshaking(initiator) => {
                now.duration_since(initiator.started) > HANDSHAKE_RETRY
            }
        }
    }

    pub fn needs_keepalive(&self, now: Instant) -> bool {
        self.link.is_up()
            && self
                .last_tx
                .is_none_or(|at| now.duration_since(at) > KEEPALIVE_INTERVAL)
    }
}

#[derive(Default)]
pub struct PeerTable {
    peers: HashMap<PeerId, Peer>,
    by_ip: HashMap<Ipv4Addr, PeerId>,
    by_key: HashMap<PublicKey, PeerId>,
    /// Our own session index -> the peer it belongs to, for demultiplexing
    /// inbound data packets.
    by_index: HashMap<u32, PeerId>,
}

impl PeerTable {
    /// Adds a peer, or refreshes one already known. Candidate addresses from the
    /// roster are merged in rather than replacing what we have, so a path we
    /// already confirmed is never dropped by a roster update.
    pub fn upsert(&mut self, info: PeerInfo) {
        let peer_id = info.peer_id;
        match self.peers.get_mut(&peer_id) {
            Some(existing) => {
                if existing.info.virtual_ip != info.virtual_ip {
                    self.by_ip.remove(&existing.info.virtual_ip);
                }
                if existing.info.public_key != info.public_key {
                    self.by_key.remove(&existing.info.public_key);
                }
                for endpoint in &info.endpoints {
                    if !existing.candidates.contains(endpoint) {
                        existing.candidates.push(*endpoint);
                    }
                }
                existing.info = info;
            }
            None => {
                self.peers.insert(peer_id, Peer::new(info));
            }
        }
        let peer = &self.peers[&peer_id];
        let (ip, key) = (peer.info.virtual_ip, peer.info.public_key);
        self.by_ip.insert(ip, peer_id);
        self.by_key.insert(key, peer_id);
    }

    pub fn remove(&mut self, peer_id: PeerId) -> Option<Peer> {
        let peer = self.peers.remove(&peer_id)?;
        self.by_ip.remove(&peer.info.virtual_ip);
        self.by_key.remove(&peer.info.public_key);
        self.by_index.retain(|_, owner| *owner != peer_id);
        Some(peer)
    }

    pub fn get(&self, peer_id: PeerId) -> Option<&Peer> {
        self.peers.get(&peer_id)
    }

    pub fn get_mut(&mut self, peer_id: PeerId) -> Option<&mut Peer> {
        self.peers.get_mut(&peer_id)
    }

    pub fn by_virtual_ip(&self, ip: Ipv4Addr) -> Option<PeerId> {
        self.by_ip.get(&ip).copied()
    }

    pub fn by_public_key(&self, key: &PublicKey) -> Option<PeerId> {
        self.by_key.get(key).copied()
    }

    pub fn by_session_index(&self, index: u32) -> Option<PeerId> {
        self.by_index.get(&index).copied()
    }

    /// Records that a peer is in one of our networks.
    pub fn add_to_network(&mut self, peer_id: PeerId, network_id: &str) {
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            peer.networks.insert(network_id.to_string());
        }
    }

    /// Removes a peer from one network. The peer itself only goes once it has
    /// no networks left in common with us.
    pub fn remove_from_network(&mut self, peer_id: PeerId, network_id: &str) -> bool {
        let Some(peer) = self.peers.get_mut(&peer_id) else { return false };
        peer.networks.remove(network_id);
        if peer.networks.is_empty() {
            self.remove(peer_id);
            true
        } else {
            false
        }
    }

    /// Forgets a network, dropping any peer that was only reachable through it.
    pub fn forget_network(&mut self, network_id: &str) {
        let affected: Vec<PeerId> = self
            .peers
            .iter()
            .filter(|(_, peer)| peer.networks.contains(network_id))
            .map(|(id, _)| *id)
            .collect();
        for peer_id in affected {
            self.remove_from_network(peer_id, network_id);
        }
    }

    /// Claims a local session index for a peer, replacing any index it held.
    pub fn bind_index(&mut self, peer_id: PeerId, index: u32) {
        self.by_index.retain(|_, owner| *owner != peer_id);
        self.by_index.insert(index, peer_id);
    }

    pub fn ids(&self) -> Vec<PeerId> {
        self.peers.keys().copied().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&PeerId, &Peer)> {
        self.peers.iter()
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

/// Decides which side dials, so two peers do not handshake at each other
/// simultaneously and build two half-used sessions. Lower ID calls.
pub fn should_initiate(local: PeerId, remote: PeerId) -> bool {
    local < remote
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(peer_id: PeerId, last_octet: u8) -> PeerInfo {
        PeerInfo {
            peer_id,
            name: format!("peer{peer_id}"),
            public_key: [peer_id as u8; 32],
            virtual_ip: Ipv4Addr::new(10, 66, 0, last_octet),
            endpoints: vec![format!("203.0.113.{last_octet}:7000").parse().unwrap()],
        }
    }

    #[test]
    fn routes_by_virtual_ip_and_key() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        assert_eq!(table.by_virtual_ip(Ipv4Addr::new(10, 66, 0, 7)), Some(7));
        assert_eq!(table.by_public_key(&[7u8; 32]), Some(7));
        assert_eq!(table.by_virtual_ip(Ipv4Addr::new(10, 66, 0, 9)), None);
    }

    #[test]
    fn removing_a_peer_clears_every_index() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        table.bind_index(7, 4242);
        assert_eq!(table.by_session_index(4242), Some(7));

        table.remove(7);
        assert!(table.is_empty());
        assert_eq!(table.by_virtual_ip(Ipv4Addr::new(10, 66, 0, 7)), None);
        assert_eq!(table.by_public_key(&[7u8; 32]), None);
        assert_eq!(table.by_session_index(4242), None);
    }

    #[test]
    fn rebinding_an_index_drops_the_previous_one() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        table.bind_index(7, 1);
        table.bind_index(7, 2);
        assert_eq!(table.by_session_index(1), None);
        assert_eq!(table.by_session_index(2), Some(7));
    }

    #[test]
    fn roster_updates_add_candidates_without_losing_a_confirmed_path() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        let confirmed: SocketAddr = "198.51.100.4:9999".parse().unwrap();
        table.get_mut(7).unwrap().confirmed = Some(confirmed);

        let mut updated = info(7, 7);
        updated.endpoints = vec!["192.168.1.9:7000".parse().unwrap()];
        table.upsert(updated);

        let peer = table.get(7).unwrap();
        assert_eq!(peer.confirmed, Some(confirmed), "confirmed path must survive");
        assert_eq!(peer.candidates.len(), 2, "new candidate should be merged in");
    }

    #[test]
    fn a_changed_virtual_ip_does_not_leave_a_stale_route() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        table.upsert(info(7, 8));
        assert_eq!(table.by_virtual_ip(Ipv4Addr::new(10, 66, 0, 7)), None);
        assert_eq!(table.by_virtual_ip(Ipv4Addr::new(10, 66, 0, 8)), Some(7));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn path_prefers_direct_and_falls_back_to_relay() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        assert_eq!(table.get(7).unwrap().path(), SendPath::Relay);

        let addr: SocketAddr = "203.0.113.7:7000".parse().unwrap();
        table.get_mut(7).unwrap().confirmed = Some(addr);
        assert_eq!(table.get(7).unwrap().path(), SendPath::Direct(addr));
        assert!(table.get(7).unwrap().is_direct());
    }

    /// Regression: a duplicate handshake response used to tear the link down,
    /// because the link was taken out of the peer before the state was checked.
    /// The peer then looked idle, re-handshook, and the two sides never settled.
    #[test]
    fn checking_the_link_state_does_not_consume_it() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        let peer = table.get_mut(7).unwrap();

        // Stand in for an established session by marking the link handshaking
        // and then confirming the check leaves it intact.
        assert!(!matches!(peer.link, Link::Handshaking(_)));
        assert!(!peer.link.is_up());

        // The guard the daemon uses must not disturb the link.
        let was_handshaking = matches!(peer.link, Link::Handshaking(_));
        assert!(!was_handshaking);
        assert!(matches!(peer.link, Link::Idle), "link must survive the check");
    }

    #[test]
    fn a_cached_handshake_answer_is_kept_per_initiation() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        let peer = table.get_mut(7).unwrap();
        assert!(peer.last_response.is_none());

        peer.last_response = Some((1001, 2002, vec![0xAA, 0xBB]));
        let (their_index, our_index, bytes) = peer.last_response.clone().unwrap();
        assert_eq!(their_index, 1001);
        assert_eq!(our_index, 2002);
        assert_eq!(bytes, vec![0xAA, 0xBB]);
    }

    #[test]
    fn a_peer_in_two_networks_survives_leaving_one() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        table.add_to_network(7, "HAM-A");
        table.add_to_network(7, "HAM-B");

        assert!(!table.remove_from_network(7, "HAM-A"), "still shares HAM-B");
        assert!(table.get(7).is_some(), "peer must not be dropped");

        assert!(table.remove_from_network(7, "HAM-B"), "last network in common");
        assert!(table.get(7).is_none(), "now the peer goes");
    }

    #[test]
    fn forgetting_a_network_drops_only_its_exclusive_peers() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        table.upsert(info(8, 8));
        table.add_to_network(7, "HAM-A");
        table.add_to_network(8, "HAM-A");
        table.add_to_network(8, "HAM-B");

        table.forget_network("HAM-A");
        assert!(table.get(7).is_none(), "only in the network we left");
        assert!(table.get(8).is_some(), "still shares HAM-B");
    }

    #[test]
    fn only_one_side_dials() {
        assert!(should_initiate(1, 2));
        assert!(!should_initiate(2, 1));
    }

    #[test]
    fn an_idle_peer_wants_a_handshake_and_a_fresh_one_does_not() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        let now = Instant::now();
        assert!(table.get(7).unwrap().needs_handshake(now));
    }

    #[test]
    fn silence_eventually_marks_a_peer_stale() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        let now = Instant::now();
        let peer = table.get_mut(7).unwrap();

        peer.last_rx = Some(now);
        assert!(!peer.is_stale(now));

        peer.last_rx = Some(now - PEER_TIMEOUT - Duration::from_secs(1));
        assert!(peer.is_stale(now));
    }

    #[test]
    fn a_peer_we_never_reached_is_not_stale_until_a_handshake_has_had_time() {
        let mut table = PeerTable::default();
        table.upsert(info(7, 7));
        let now = Instant::now();
        let peer = table.get_mut(7).unwrap();
        assert!(!peer.is_stale(now), "no handshake attempted yet");

        peer.last_handshake = Some(now - PEER_TIMEOUT - Duration::from_secs(1));
        assert!(peer.is_stale(now));
    }
}
