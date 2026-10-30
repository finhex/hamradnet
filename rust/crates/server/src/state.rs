//! Coordinator state: who is connected, which networks exist, who is in them.
//!
//! All mutation happens behind a single mutex. Critical sections are short and
//! never await. Notifications to other peers go out through unbounded channels,
//! whose `send` never blocks, so it is safe to push them while holding the lock.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use hamradnet_proto::{
    control::{generate_network_id, NetworkInfo, PeerInfo},
    wire::RELAY_TOKEN_LEN,
    ErrorCode, PeerId, PublicKey, ServerMsg, VIRTUAL_PREFIX_LEN,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::UnboundedSender;

/// Refuse to grow a single network past this. Broadcast traffic is fanned out to
/// every member, so an unbounded network would be an amplification vector.
pub const MAX_PEERS_PER_NETWORK: usize = 256;

/// A network nobody has joined for this long is dropped, even if it has never
/// been occupied at all.
///
/// In practice this catches networks created and then abandoned before anyone
/// joined them: one that has been used is already removed the moment its last
/// member leaves.
pub const ABANDONED_AFTER: u64 = 14 * 24 * 60 * 60;

/// How long a network that has emptied out is kept before being removed.
///
/// Deleting the instant the last member drops is the literal reading of "remove
/// it when everyone has left", but it also destroys a network every time its
/// only member closes the app for the evening - and closing the app is not the
/// same thing as abandoning the network. Fourteen days empty is the point at
/// which nobody is coming back, and it matches the rule for networks that were
/// never joined at all, so there is only one number to reason about.
pub const DEFAULT_EMPTY_GRACE: u64 = ABANDONED_AFTER;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A connected client.
pub struct Peer {
    pub id: PeerId,
    pub name: String,
    pub public_key: PublicKey,
    pub virtual_ip: Ipv4Addr,
    /// Candidates the client reported itself (LAN interface addresses).
    pub reported_endpoints: Vec<SocketAddr>,
    /// Source address the server saw on the UDP plane - the peer's public
    /// address after NAT. Far more useful than anything the client can report.
    pub observed_endpoint: Option<SocketAddr>,
    pub networks: HashSet<String>,
    pub relay_token: [u8; RELAY_TOKEN_LEN],
    pub tx: UnboundedSender<ServerMsg>,
}

impl Peer {
    /// Candidate list handed to other peers: observed address first, since a
    /// direct path through NAT is the one most likely to work.
    pub fn candidates(&self) -> Vec<SocketAddr> {
        let mut out = Vec::with_capacity(self.reported_endpoints.len() + 1);
        if let Some(observed) = self.observed_endpoint {
            out.push(observed);
        }
        for endpoint in &self.reported_endpoints {
            if !out.contains(endpoint) {
                out.push(*endpoint);
            }
        }
        out
    }

    pub fn info(&self) -> PeerInfo {
        PeerInfo {
            peer_id: self.id,
            name: self.name.clone(),
            public_key: self.public_key,
            virtual_ip: self.virtual_ip,
            endpoints: self.candidates(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Network {
    pub id: String,
    pub name: String,
    #[serde(with = "hex_bytes")]
    pub salt: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub password_hash: Vec<u8>,
    /// Public key of whoever created it. All zeroes for networks created before
    /// ownership was recorded, which any member may delete.
    #[serde(default, with = "hex_bytes")]
    pub owner: Vec<u8>,
    /// Unix seconds when somebody last joined. Drives expiry.
    #[serde(default)]
    pub last_active: u64,
    /// Whether anyone has ever been a member.
    ///
    /// A network is memberless for the moment between being created and its
    /// creator joining, and deleting it in that window would be a race the
    /// client could never win. Only networks that have actually been occupied
    /// are subject to the empty-network rule.
    #[serde(default)]
    pub ever_joined: bool,
    /// Unix seconds since the last member left, if it is currently empty.
    #[serde(skip)]
    pub empty_since: Option<u64>,
    #[serde(skip)]
    pub members: HashSet<PeerId>,
}

impl Network {
    fn has_owner(&self) -> bool {
        self.owner.len() == 32 && self.owner.iter().any(|b| *b != 0)
    }
}

impl Network {
    fn new(name: String, password: &str, owner: PublicKey, rng: &mut impl Rng) -> Self {
        let salt: [u8; 16] = rng.gen();
        Self {
            id: generate_network_id(rng),
            name,
            password_hash: hash_password(&salt, password).to_vec(),
            salt: salt.to_vec(),
            owner: owner.to_vec(),
            last_active: now_secs(),
            ever_joined: false,
            empty_since: None,
            members: HashSet::new(),
        }
    }

    fn password_matches(&self, password: &str) -> bool {
        let candidate = hash_password(&self.salt, password);
        constant_time_eq(&candidate, &self.password_hash)
    }
}

/// Salted SHA-256. This guards a game-night network ID, not a bank account; the
/// threat it defends against is a passer-by who learned a network ID, and it is
/// never exposed to an offline attacker unless the server itself is breached.
fn hash_password(salt: &[u8], password: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(password.as_bytes());
    hasher.finalize().into()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Hands out addresses inside 10.66.0.0/16, skipping .0 (network), .1 (reserved
/// for the server's own use) and the broadcast address.
pub struct IpAllocator {
    next: u32,
    released: Vec<u32>,
}

impl Default for IpAllocator {
    fn default() -> Self {
        Self { next: 2, released: Vec::new() }
    }
}

impl IpAllocator {
    /// 10.66.255.255 is the broadcast address, so the last usable host is .254.
    const LAST: u32 = 0xFFFE;

    pub fn allocate(&mut self) -> Option<Ipv4Addr> {
        let host = self.released.pop().or_else(|| {
            if self.next > Self::LAST {
                None
            } else {
                let value = self.next;
                self.next += 1;
                Some(value)
            }
        })?;
        Some(Ipv4Addr::new(10, 66, (host >> 8) as u8, (host & 0xFF) as u8))
    }

    pub fn release(&mut self, addr: Ipv4Addr) {
        let octets = addr.octets();
        self.released.push(((octets[2] as u32) << 8) | octets[3] as u32);
    }
}

#[derive(Default)]
pub struct State {
    pub peers: HashMap<PeerId, Peer>,
    pub networks: HashMap<String, Network>,
    /// UDP source address registered by each peer, used to route relayed traffic.
    pub relay_addrs: HashMap<PeerId, SocketAddr>,
    /// Reverse of `relay_addrs`, so an incoming datagram can be attributed to a
    /// peer without scanning every entry.
    relay_by_addr: HashMap<SocketAddr, PeerId>,
    next_peer_id: PeerId,
    ips: IpAllocator,
    store_path: Option<PathBuf>,
    /// Seconds an empty network is kept before removal.
    empty_grace: u64,
}

impl State {
    /// Test convenience: the default grace period. Production builds construct
    /// the state with the grace taken from the command line.
    #[cfg(test)]
    pub fn with_store(path: Option<PathBuf>) -> Self {
        Self::with_store_and_grace(path, DEFAULT_EMPTY_GRACE)
    }

    pub fn with_store_and_grace(path: Option<PathBuf>, empty_grace: u64) -> Self {
        let mut state = State {
            next_peer_id: 1,
            store_path: path,
            empty_grace,
            ..Default::default()
        };
        if let Some(path) = state.store_path.clone() {
            match load_networks(&path) {
                Ok(networks) => {
                    tracing::info!(count = networks.len(), "loaded persisted networks");
                    for mut network in networks {
                        // Entries written before expiry existed have no
                        // timestamp; start their clock now rather than treating
                        // them as decades old and deleting them immediately.
                        if network.last_active == 0 {
                            network.last_active = now_secs();
                        }
                        // Nobody is connected yet after a restart, so an
                        // occupied network starts its empty clock here and is
                        // reprieved as soon as someone reconnects.
                        if network.ever_joined {
                            network.empty_since = Some(now_secs());
                        }
                        state.networks.insert(network.id.clone(), network);
                    }
                }
                Err(err) => tracing::warn!(%err, "could not load network store, starting empty"),
            }
        }
        state
    }

    pub fn register_peer(
        &mut self,
        name: String,
        public_key: PublicKey,
        tx: UnboundedSender<ServerMsg>,
    ) -> Option<(PeerId, [u8; RELAY_TOKEN_LEN])> {
        let virtual_ip = self.ips.allocate()?;
        let id = self.next_peer_id;
        self.next_peer_id = self.next_peer_id.wrapping_add(1).max(1);
        let relay_token: [u8; RELAY_TOKEN_LEN] = rand::thread_rng().gen();
        self.peers.insert(
            id,
            Peer {
                id,
                name,
                public_key,
                virtual_ip,
                reported_endpoints: Vec::new(),
                observed_endpoint: None,
                networks: HashSet::new(),
                relay_token,
                tx,
            },
        );
        Some((id, relay_token))
    }

    /// Drops a peer and tells every network it belonged to.
    pub fn remove_peer(&mut self, peer_id: PeerId) {
        let Some(peer) = self.peers.remove(&peer_id) else { return };
        self.ips.release(peer.virtual_ip);
        if let Some(addr) = self.relay_addrs.remove(&peer_id) {
            self.relay_by_addr.remove(&addr);
        }
        for network_id in &peer.networks {
            if let Some(network) = self.networks.get_mut(network_id) {
                network.members.remove(&peer_id);
                let members: Vec<PeerId> = network.members.iter().copied().collect();
                for member in members {
                    self.notify(
                        member,
                        ServerMsg::PeerLeft { network_id: network_id.clone(), peer_id },
                    );
                }
            }
        }
        // Dropping off counts as leaving, so the last one out closes the network.
        for network_id in peer.networks.iter().cloned().collect::<Vec<_>>() {
            self.mark_if_empty(&network_id);
        }
        tracing::info!(peer_id, name = %peer.name, "peer disconnected");
    }

    pub fn create_network(&mut self, name: String, password: &str, owner: PublicKey) -> String {
        let network = Network::new(name, password, owner, &mut rand::thread_rng());
        let id = network.id.clone();
        self.networks.insert(id.clone(), network);
        self.persist();
        id
    }

    /// Removes a network for everyone, if the asker is entitled to.
    pub fn delete_network(&mut self, peer_id: PeerId, network_id: &str) -> Result<(), ErrorCode> {
        let requester = self.peers.get(&peer_id).ok_or(ErrorCode::Internal)?.public_key;
        let network = self.networks.get(network_id).ok_or(ErrorCode::NoSuchNetwork)?;

        if network.has_owner() {
            if network.owner != requester {
                return Err(ErrorCode::NotOwner);
            }
        } else if !network.members.contains(&peer_id) {
            // No owner on record, so fall back to requiring membership, which
            // means having produced the password.
            return Err(ErrorCode::NotOwner);
        }

        let members: Vec<PeerId> = network.members.iter().copied().collect();
        self.networks.remove(network_id);
        for member in members {
            if let Some(peer) = self.peers.get_mut(&member) {
                peer.networks.remove(network_id);
            }
            self.notify(member, ServerMsg::Deleted { network_id: network_id.to_string() });
        }
        self.persist();
        tracing::info!(peer_id, network_id, "network deleted");
        Ok(())
    }



    pub fn join_network(
        &mut self,
        peer_id: PeerId,
        network_id: &str,
        password: &str,
    ) -> Result<(NetworkInfo, Vec<PeerInfo>), ErrorCode> {
        let network = self.networks.get(network_id).ok_or(ErrorCode::NoSuchNetwork)?;
        if !network.password_matches(password) {
            return Err(ErrorCode::BadPassword);
        }
        if network.members.contains(&peer_id) {
            return Err(ErrorCode::AlreadyJoined);
        }
        if network.members.len() >= MAX_PEERS_PER_NETWORK {
            return Err(ErrorCode::NetworkFull);
        }

        let existing: Vec<PeerId> = network.members.iter().copied().collect();
        let network_name = network.name.clone();

        let joiner_info = {
            let peer = self.peers.get_mut(&peer_id).ok_or(ErrorCode::Internal)?;
            peer.networks.insert(network_id.to_string());
            peer.info()
        };
        {
            let network = self.networks.get_mut(network_id).ok_or(ErrorCode::Internal)?;
            network.members.insert(peer_id);
            // Any join resets the expiry clock and cancels a pending removal.
            network.last_active = now_secs();
            network.ever_joined = true;
            network.empty_since = None;
        }
        self.persist();

        // Tell everyone already in the network about the newcomer.
        for member in &existing {
            self.notify(
                *member,
                ServerMsg::PeerUpdate {
                    network_id: network_id.to_string(),
                    peer: joiner_info.clone(),
                },
            );
        }

        let roster = existing
            .iter()
            .filter_map(|id| self.peers.get(id).map(Peer::info))
            .collect();

        tracing::info!(peer_id, network_id, name = %network_name, "peer joined network");
        Ok((
            NetworkInfo {
                network_id: network_id.to_string(),
                name: network_name,
                virtual_ip: joiner_info.virtual_ip,
                prefix_len: VIRTUAL_PREFIX_LEN,
            },
            roster,
        ))
    }

    pub fn leave_network(&mut self, peer_id: PeerId, network_id: &str) {
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            peer.networks.remove(network_id);
        }
        let Some(network) = self.networks.get_mut(network_id) else { return };
        network.members.remove(&peer_id);
        let members: Vec<PeerId> = network.members.iter().copied().collect();
        for member in members {
            self.notify(
                member,
                ServerMsg::PeerLeft { network_id: network_id.to_string(), peer_id },
            );
        }
        self.mark_if_empty(network_id);
    }

    /// Starts the removal clock when the last member goes, and removes the
    /// network straight away if no grace period is configured.
    fn mark_if_empty(&mut self, network_id: &str) {
        let remove_now = {
            let Some(network) = self.networks.get_mut(network_id) else { return };
            if !network.members.is_empty() || !network.ever_joined {
                return;
            }
            network.empty_since = Some(now_secs());
            self.empty_grace == 0
        };
        if remove_now {
            tracing::info!(network_id, "everyone left, removing the network");
            self.networks.remove(network_id);
            self.persist();
        }
    }

    /// Removes networks whose members have all gone, once the grace period has
    /// passed. Also clears out networks that were created but never joined.
    pub fn sweep_empty(&mut self) -> usize {
        let now = now_secs();
        let grace = self.empty_grace;
        let doomed: Vec<String> = self
            .networks
            .values()
            .filter(|network| {
                if !network.members.is_empty() {
                    return false;
                }
                match network.empty_since {
                    // Emptied out: remove once the grace period is up.
                    Some(since) => now.saturating_sub(since) >= grace,
                    // Never occupied: only the long abandonment rule applies, so
                    // that creating a network and joining it is not a race.
                    None => {
                        !network.ever_joined
                            && now.saturating_sub(network.last_active) >= ABANDONED_AFTER
                    }
                }
            })
            .map(|network| network.id.clone())
            .collect();

        for id in &doomed {
            tracing::info!(network_id = %id, "removing an empty network");
            self.networks.remove(id);
        }
        if !doomed.is_empty() {
            self.persist();
        }
        doomed.len()
    }

    /// Records new candidate addresses and republishes them to the peer's
    /// networks. Returns true if anything actually changed, so callers can skip
    /// pointless broadcasts.
    pub fn update_endpoints(
        &mut self,
        peer_id: PeerId,
        reported: Option<Vec<SocketAddr>>,
        observed: Option<SocketAddr>,
    ) -> bool {
        let Some(peer) = self.peers.get_mut(&peer_id) else { return false };
        let mut changed = false;
        if let Some(reported) = reported {
            if reported != peer.reported_endpoints {
                peer.reported_endpoints = reported;
                changed = true;
            }
        }
        if let Some(observed) = observed {
            if peer.observed_endpoint != Some(observed) {
                peer.observed_endpoint = Some(observed);
                changed = true;
            }
        }
        if !changed {
            return false;
        }

        let info = peer.info();
        let networks: Vec<String> = peer.networks.iter().cloned().collect();
        for network_id in networks {
            let Some(network) = self.networks.get(&network_id) else { continue };
            let members: Vec<PeerId> =
                network.members.iter().copied().filter(|id| *id != peer_id).collect();
            for member in members {
                self.notify(
                    member,
                    ServerMsg::PeerUpdate { network_id: network_id.clone(), peer: info.clone() },
                );
            }
        }
        true
    }

    /// Binds a UDP source address to a peer, but only if the datagram carried
    /// the relay token that peer was issued over its authenticated control
    /// connection. Without this check anyone could claim any peer's ID and have
    /// its relayed traffic delivered to them.
    pub fn register_relay(&mut self, peer_id: PeerId, addr: SocketAddr, token: &[u8]) -> bool {
        let Some(peer) = self.peers.get(&peer_id) else { return false };
        if !constant_time_eq(token, &peer.relay_token) {
            tracing::warn!(peer_id, %addr, "relay registration with a bad token");
            return false;
        }
        if let Some(previous) = self.relay_addrs.insert(peer_id, addr) {
            if previous != addr {
                self.relay_by_addr.remove(&previous);
            }
        }
        self.relay_by_addr.insert(addr, peer_id);
        true
    }

    /// Which peer, if any, owns this UDP source address.
    pub fn peer_at(&self, addr: &SocketAddr) -> Option<PeerId> {
        self.relay_by_addr.get(addr).copied()
    }

    /// Gate on the relay: two peers may exchange traffic only if they are in a
    /// network together.
    pub fn shares_network(&self, a: PeerId, b: PeerId) -> bool {
        match (self.peers.get(&a), self.peers.get(&b)) {
            (Some(x), Some(y)) => !x.networks.is_disjoint(&y.networks),
            _ => false,
        }
    }

    /// Fire-and-forget: a closed channel just means the peer is on its way out
    /// and its disconnect path will clean up.
    fn notify(&self, peer_id: PeerId, msg: ServerMsg) {
        if let Some(peer) = self.peers.get(&peer_id) {
            let _ = peer.tx.send(msg);
        }
    }

    fn persist(&self) {
        let Some(path) = &self.store_path else { return };
        let networks: Vec<&Network> = self.networks.values().collect();
        match serde_json::to_vec_pretty(&networks) {
            Ok(bytes) => {
                if let Err(err) = std::fs::write(path, bytes) {
                    tracing::warn!(%err, "could not persist networks");
                }
            }
            Err(err) => tracing::warn!(%err, "could not serialise networks"),
        }
    }
}

fn load_networks(path: &Path) -> anyhow::Result<Vec<Network>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        hex::encode(bytes).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        hex::decode(text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn test_peer(state: &mut State, name: &str) -> PeerId {
        keyed_peer(state, name, [0u8; 32])
    }

    fn keyed_peer(state: &mut State, name: &str, key: PublicKey) -> PeerId {
        let (tx, rx) = unbounded_channel();
        // Keep the receiver alive for the duration of the test.
        Box::leak(Box::new(rx));
        state.register_peer(name.into(), key, tx).unwrap().0
    }

    #[test]
    fn allocator_skips_reserved_addresses_and_reuses_released() {
        let mut alloc = IpAllocator::default();
        assert_eq!(alloc.allocate().unwrap(), Ipv4Addr::new(10, 66, 0, 2));
        let second = alloc.allocate().unwrap();
        assert_eq!(second, Ipv4Addr::new(10, 66, 0, 3));
        alloc.release(second);
        assert_eq!(alloc.allocate().unwrap(), second);
    }

    #[test]
    fn allocator_crosses_octet_boundary() {
        let mut alloc = IpAllocator::default();
        let mut last = Ipv4Addr::UNSPECIFIED;
        for _ in 0..300 {
            last = alloc.allocate().unwrap();
        }
        assert_eq!(last, Ipv4Addr::new(10, 66, 1, 45));
    }

    #[test]
    fn wrong_password_is_rejected() {
        let mut state = State::with_store(None);
        let id = state.create_network("games".into(), "hunter2", [1u8; 32]);
        let peer = test_peer(&mut state, "a");
        assert_eq!(
            state.join_network(peer, &id, "wrong").unwrap_err(),
            ErrorCode::BadPassword
        );
        assert!(state.join_network(peer, &id, "hunter2").is_ok());
    }

    #[test]
    fn unknown_network_is_rejected() {
        let mut state = State::with_store(None);
        let peer = test_peer(&mut state, "a");
        assert_eq!(
            state.join_network(peer, "HAM-0000-0000", "x").unwrap_err(),
            ErrorCode::NoSuchNetwork
        );
    }

    #[test]
    fn joining_twice_is_rejected() {
        let mut state = State::with_store(None);
        let id = state.create_network("games".into(), "pw", [1u8; 32]);
        let peer = test_peer(&mut state, "a");
        state.join_network(peer, &id, "pw").unwrap();
        assert_eq!(
            state.join_network(peer, &id, "pw").unwrap_err(),
            ErrorCode::AlreadyJoined
        );
    }

    #[test]
    fn second_peer_sees_the_first_in_its_roster() {
        let mut state = State::with_store(None);
        let id = state.create_network("games".into(), "pw", [1u8; 32]);
        let first = test_peer(&mut state, "first");
        let second = test_peer(&mut state, "second");

        let (_, roster) = state.join_network(first, &id, "pw").unwrap();
        assert!(roster.is_empty(), "first peer should see an empty network");

        let (info, roster) = state.join_network(second, &id, "pw").unwrap();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].peer_id, first);
        assert_ne!(info.virtual_ip, roster[0].virtual_ip, "IPs must be unique");
    }

    #[test]
    fn removing_a_peer_frees_its_ip_and_membership() {
        let mut state = State::with_store(None);
        let id = state.create_network("games".into(), "pw", [1u8; 32]);
        let peer = test_peer(&mut state, "a");
        let ip = state.peers[&peer].virtual_ip;
        state.join_network(peer, &id, "pw").unwrap();

        state.remove_peer(peer);
        assert!(!state.peers.contains_key(&peer));
        assert!(state.networks[&id].members.is_empty());

        let next = test_peer(&mut state, "b");
        assert_eq!(state.peers[&next].virtual_ip, ip, "IP should be recycled");
    }

    #[test]
    fn the_creator_can_delete_and_others_cannot() {
        let mut state = State::with_store(None);
        let owner_key = [9u8; 32];
        let owner = keyed_peer(&mut state, "owner", owner_key);
        let guest = keyed_peer(&mut state, "guest", [3u8; 32]);

        let id = state.create_network("games".into(), "pw", owner_key);
        state.join_network(owner, &id, "pw").unwrap();
        state.join_network(guest, &id, "pw").unwrap();

        // A member who did not create it is refused, even knowing the password.
        assert_eq!(state.delete_network(guest, &id).unwrap_err(), ErrorCode::NotOwner);
        assert!(state.networks.contains_key(&id));

        assert!(state.delete_network(owner, &id).is_ok());
        assert!(!state.networks.contains_key(&id));
        // Membership is cleaned up on both sides.
        assert!(state.peers[&owner].networks.is_empty());
        assert!(state.peers[&guest].networks.is_empty());
    }

    #[test]
    fn deleting_an_unknown_network_is_refused() {
        let mut state = State::with_store(None);
        let peer = test_peer(&mut state, "a");
        assert_eq!(
            state.delete_network(peer, "HAM-0000-0000").unwrap_err(),
            ErrorCode::NoSuchNetwork
        );
    }

    /// Networks with no owner recorded predate ownership; a member may remove
    /// them, but a stranger may not.
    #[test]
    fn a_legacy_network_can_be_deleted_by_a_member_only() {
        let mut state = State::with_store(None);
        let member = test_peer(&mut state, "member");
        let stranger = keyed_peer(&mut state, "stranger", [4u8; 32]);

        let id = state.create_network("old".into(), "pw", [0u8; 32]);
        state.join_network(member, &id, "pw").unwrap();

        assert_eq!(
            state.delete_network(stranger, &id).unwrap_err(),
            ErrorCode::NotOwner
        );
        assert!(state.delete_network(member, &id).is_ok());
    }

    #[test]
    fn an_occupied_network_is_never_removed() {
        let mut state = State::with_store_and_grace(None, 0);
        let peer = test_peer(&mut state, "a");
        let id = state.create_network("games".into(), "pw", [1u8; 32]);
        state.join_network(peer, &id, "pw").unwrap();

        assert_eq!(state.sweep_empty(), 0);
        assert!(state.networks.contains_key(&id));
    }

    /// The behaviour asked for: the last person out closes the network.
    #[test]
    fn the_network_goes_when_the_last_member_leaves() {
        let mut state = State::with_store_and_grace(None, 0);
        let first = test_peer(&mut state, "first");
        let second = test_peer(&mut state, "second");
        let id = state.create_network("games".into(), "pw", [1u8; 32]);
        state.join_network(first, &id, "pw").unwrap();
        state.join_network(second, &id, "pw").unwrap();

        state.leave_network(first, &id);
        assert!(state.networks.contains_key(&id), "someone is still in it");

        state.leave_network(second, &id);
        assert!(!state.networks.contains_key(&id), "the last one out closes it");
    }

    #[test]
    fn disconnecting_counts_as_leaving() {
        let mut state = State::with_store_and_grace(None, 0);
        let peer = test_peer(&mut state, "a");
        let id = state.create_network("games".into(), "pw", [1u8; 32]);
        state.join_network(peer, &id, "pw").unwrap();

        state.remove_peer(peer);
        assert!(!state.networks.contains_key(&id));
    }

    /// A network is memberless between being created and its creator joining.
    /// Removing it in that gap would be a race the client could never win.
    #[test]
    fn a_freshly_created_network_survives_until_its_creator_joins() {
        let mut state = State::with_store_and_grace(None, 0);
        let peer = test_peer(&mut state, "a");
        let id = state.create_network("games".into(), "pw", [1u8; 32]);

        assert_eq!(state.sweep_empty(), 0, "nobody has ever joined it yet");
        assert!(state.networks.contains_key(&id));
        assert!(state.join_network(peer, &id, "pw").is_ok());
    }

    #[test]
    fn a_grace_period_holds_the_network_briefly() {
        let mut state = State::with_store_and_grace(None, 60);
        let peer = test_peer(&mut state, "a");
        let id = state.create_network("games".into(), "pw", [1u8; 32]);
        state.join_network(peer, &id, "pw").unwrap();

        state.leave_network(peer, &id);
        assert!(state.networks.contains_key(&id), "held during the grace period");
        assert_eq!(state.sweep_empty(), 0);

        // Once the grace has elapsed it goes.
        state.networks.get_mut(&id).unwrap().empty_since = Some(now_secs() - 61);
        assert_eq!(state.sweep_empty(), 1);
        assert!(!state.networks.contains_key(&id));
    }

    #[test]
    fn reconnecting_within_the_grace_period_saves_the_network() {
        let mut state = State::with_store_and_grace(None, 60);
        let peer = test_peer(&mut state, "a");
        let id = state.create_network("games".into(), "pw", [1u8; 32]);
        state.join_network(peer, &id, "pw").unwrap();
        state.leave_network(peer, &id);

        // Back before the grace runs out, as after a dropped link.
        let returning = test_peer(&mut state, "a-again");
        assert!(state.join_network(returning, &id, "pw").is_ok());
        assert_eq!(state.networks[&id].empty_since, None, "removal is cancelled");
        assert_eq!(state.sweep_empty(), 0);
    }

    #[test]
    fn a_network_created_and_never_joined_is_eventually_cleaned_up() {
        let mut state = State::with_store_and_grace(None, 0);
        let id = state.create_network("games".into(), "pw", [1u8; 32]);
        state.networks.get_mut(&id).unwrap().last_active =
            now_secs().saturating_sub(ABANDONED_AFTER + 1);
        assert_eq!(state.sweep_empty(), 1);
    }

    #[test]
    fn observed_endpoint_is_offered_before_reported_ones() {
        let mut state = State::with_store(None);
        let peer = test_peer(&mut state, "a");
        let lan: SocketAddr = "192.168.1.5:7000".parse().unwrap();
        let public: SocketAddr = "203.0.113.7:41234".parse().unwrap();

        assert!(state.update_endpoints(peer, Some(vec![lan]), Some(public)));
        assert_eq!(state.peers[&peer].candidates(), vec![public, lan]);
        // Repeating the same information is not a change.
        assert!(!state.update_endpoints(peer, Some(vec![lan]), Some(public)));
    }
}
