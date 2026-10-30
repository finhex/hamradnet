//! The daemon: owns the tunnel interface, the UDP socket, the peer table and
//! the connection to the coordinator, and moves packets between them.
//!
//! Threading:
//!   * two OS threads drive the tun device, because its reads block;
//!   * everything else is tokio tasks sharing one `Mutex<Inner>`.
//!
//! The mutex is a plain `std::sync::Mutex` and is never held across an `await`.
//! Handlers lock, compute the datagrams to send, unlock, and only then send.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hamradnet_proto::{
    wire::Packet, ClientMsg, NetworkInfo, PeerId, PeerInfo, ServerMsg, MAX_PACKET_SIZE,
    PROTOCOL_VERSION, TUN_MTU,
};
use rand::Rng;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc};

use crate::config::Config;
use crate::framing::{read_frame, write_frame};
use crate::ip;
use crate::ipc;
use crate::peers::{should_initiate, Link, PeerTable, SendPath};
use crate::session;
use crate::tun::{Tun, TunConfig};

/// Name of the virtual interface.
pub const INTERFACE_NAME: &str = "hamrad0";
/// How often the tick loop runs. Everything slower is a multiple of this.
const TICK: Duration = Duration::from_secs(1);
/// How often we re-register with the relay and re-check our public address.
const SERVER_REFRESH: Duration = Duration::from_secs(15);
/// Control-plane keepalive.
const CONTROL_PING: Duration = Duration::from_secs(30);
/// Consecutive read failures that mean the interface has gone, rather than one
/// packet having been unreadable.
const READ_FAILURE_LIMIT: u32 = 64;

/// What the coordinator told us about ourselves.
#[derive(Debug, Clone)]
pub struct Identity {
    pub peer_id: PeerId,
    pub virtual_ip: Ipv4Addr,
    pub prefix_len: u8,
    pub relay_token: [u8; hamradnet_proto::wire::RELAY_TOKEN_LEN],
    pub server_udp: SocketAddr,
}

/// Broadcast to whoever is waiting on the result of a CLI command.
#[derive(Debug, Clone)]
pub enum Event {
    Created { network_id: String, name: String },
    Joined { network_id: String, name: String },
    Left { network_id: String },
    Deleted { network_id: String },
    Failed { message: String },
}

pub struct Inner {
    pub config: Config,
    pub config_path: PathBuf,
    pub peers: PeerTable,
    pub identity: Option<Identity>,
    pub networks: HashMap<String, NetworkInfo>,
    /// Passwords for joins in flight, kept so a successful join can be saved.
    pub pending_joins: HashMap<String, String>,
    /// Our address as the server sees it.
    pub public_endpoint: Option<SocketAddr>,
    /// Our address on the local network, for peers in the same house.
    pub local_endpoint: Option<SocketAddr>,
    pub connected: bool,
    /// Round trip to the coordinator, measured by the reflection probe.
    pub server_rtt: Option<Duration>,
    /// The reflection probe awaiting a reply, as `(txid, when it was sent)`.
    pub reflect_inflight: Option<(u64, Instant)>,
    /// How many probes have been answered. The first is thrown away.
    pub reflect_samples: u32,
    /// Why the last connection attempt failed, for the UI to show. Cleared once
    /// a connection succeeds.
    pub last_error: Option<String>,
}

impl Inner {
    fn candidates_to_publish(&self) -> Vec<SocketAddr> {
        let mut out = Vec::new();
        if let Some(local) = self.local_endpoint {
            out.push(local);
        }
        out
    }
}

/// Keeps our relay mapping alive and re-checks our public address. The probe id
/// is returned so the reply can be timed.
fn server_refresh_datagrams(identity: &Identity) -> (Vec<Datagram>, u64) {
    let txid: u64 = rand::thread_rng().gen();
    let datagrams = vec![
        Datagram {
            data: Packet::RelayRegister {
                peer_id: identity.peer_id,
                token: identity.relay_token,
            }
            .to_vec(),
            dest: identity.server_udp,
        },
        Datagram {
            data: Packet::ReflectRequest { txid }.to_vec(),
            dest: identity.server_udp,
        },
    ];
    (datagrams, txid)
}

/// One datagram waiting to go out.
pub struct Datagram {
    pub data: Vec<u8>,
    pub dest: SocketAddr,
}

/// How an inbound packet reached us. Only a direct arrival may confirm a path.
#[derive(Debug, Clone, Copy)]
enum Via {
    Direct(SocketAddr),
    Relay,
}

#[derive(Clone)]
pub struct Daemon {
    inner: Arc<Mutex<Inner>>,
    socket: Arc<UdpSocket>,
    /// Packets destined for the tun device.
    to_tun: mpsc::UnboundedSender<Vec<u8>>,
    /// Control-plane messages destined for the server.
    to_server: mpsc::UnboundedSender<ClientMsg>,
    events: broadcast::Sender<Event>,
    /// Held so the interface can be torn down on shutdown.
    tun: Arc<Mutex<Option<Arc<Tun>>>>,
    tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Daemon {
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn inner(&self) -> &Arc<Mutex<Inner>> {
        &self.inner
    }

    pub fn send_to_server(&self, msg: ClientMsg) {
        let _ = self.to_server.send(msg);
    }

    /// Registers with the relay and asks the server what our public address
    /// looks like.
    ///
    /// Called as soon as we have an identity, not just on the periodic refresh:
    /// until this lands the server has nowhere to deliver our relayed traffic,
    /// so a peer pair that cannot punch would sit unable to connect until the
    /// first refresh came round.
    pub async fn announce_to_relay(&self) {
        let datagrams = {
            let mut guard = self.inner.lock().unwrap();
            let Some(identity) = guard.identity.clone() else { return };
            let (datagrams, txid) = server_refresh_datagrams(&identity);
            guard.reflect_inflight = Some((txid, Instant::now()));
            datagrams
        };
        self.flush(datagrams).await;
    }

    /// Stops everything: background tasks, the tunnel interface, and the thread
    /// blocked reading from it. Used when switching to a different coordinator,
    /// so the app does not have to be restarted.
    pub fn shutdown(&self) {
        for handle in self.tasks.lock().unwrap().drain(..) {
            handle.abort();
        }
        // Closing the device releases the reader thread, which is sitting in a
        // blocking read and cannot be cancelled any other way.
        if let Some(tun) = self.tun.lock().unwrap().take() {
            tun.stop();
        }
        tracing::info!("daemon stopped");
    }

    async fn flush(&self, datagrams: Vec<Datagram>) {
        for datagram in datagrams {
            if let Err(err) = self.socket.send_to(&datagram.data, datagram.dest).await {
                tracing::trace!(%err, dest = %datagram.dest, "udp send failed");
            }
        }
    }
}

/// Entry point. Runs until the process is stopped.
/// Starts the daemon and hands back a handle to it.
///
/// Everything runs as background tasks, so a caller that wants to drive the
/// daemon rather than just host it - the GUI, which embeds it in-process -
/// keeps the handle and uses it directly instead of going over the local socket.
pub async fn spawn(config: Config, config_path: PathBuf, udp_port: u16) -> Result<Daemon> {
    let socket = Arc::new(
        UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), udp_port))
            .await
            .context("binding the local UDP socket")?,
    );
    let bound = socket.local_addr()?;
    tracing::info!(%bound, "udp socket bound");

    let (to_tun, tun_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (to_server, server_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let (events, _) = broadcast::channel(64);

    let server = config.server.clone();
    let inner = Arc::new(Mutex::new(Inner {
        config,
        config_path,
        peers: PeerTable::default(),
        identity: None,
        networks: HashMap::new(),
        pending_joins: HashMap::new(),
        public_endpoint: None,
        local_endpoint: None,
        connected: false,
        server_rtt: None,
        reflect_inflight: None,
        reflect_samples: 0,
        last_error: None,
    }));

    // The tun device is created once the coordinator has assigned an address.
    let tun_slot: Arc<Mutex<Option<Arc<Tun>>>> = Arc::new(Mutex::new(None));
    // Handed to the writer thread when the device is created.
    let tun_rx_slot = Arc::new(Mutex::new(Some(tun_rx)));
    let tasks = Arc::new(Mutex::new(Vec::new()));

    let daemon = Daemon {
        inner,
        socket: Arc::clone(&socket),
        to_tun,
        to_server,
        events,
        tun: Arc::clone(&tun_slot),
        tasks: Arc::clone(&tasks),
    };
    let server_queue: ServerQueue = Arc::new(tokio::sync::Mutex::new(server_rx));

    *tasks.lock().unwrap() = vec![
        tokio::spawn(udp_loop(daemon.clone())),
        tokio::spawn(tick_loop(daemon.clone())),
        tokio::spawn(crate::ipc_server::serve(daemon.clone())),
        tokio::spawn(reconnect_loop(
            daemon.clone(),
            server,
            tun_slot,
            tun_rx_slot,
            bound.port(),
            server_queue,
        )),
    ];

    Ok(daemon)
}

/// Hosts the daemon for the lifetime of the process, for the CLI's `up`.
pub async fn run(config: Config, config_path: PathBuf, udp_port: u16) -> Result<()> {
    let _daemon = spawn(config, config_path, udp_port).await?;
    // The work happens in background tasks; block here until the process stops.
    std::future::pending::<()>().await;
    Ok(())
}

/// Reconnects forever; each pass rebuilds the peer table from a fresh roster.
#[allow(clippy::too_many_arguments)]
async fn reconnect_loop(
    daemon: Daemon,
    server: String,
    tun_slot: Arc<Mutex<Option<Arc<Tun>>>>,
    tun_rx_slot: Arc<Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>>,
    local_udp_port: u16,
    server_queue: ServerQueue,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match control_session(
            &daemon,
            &server,
            &tun_slot,
            &tun_rx_slot,
            local_udp_port,
            Arc::clone(&server_queue),
        )
        .await
        {
            // A connection that came up and later dropped is not a reason to
            // keep backing off, so the next attempt starts from the bottom.
            Ok(()) => {
                tracing::warn!("control connection closed");
                backoff = Duration::from_secs(1);
            }
            Err(err) => {
                tracing::warn!(%err, "control connection failed");
                daemon.inner.lock().unwrap().last_error = Some(format!("{err:#}"));
            }
        }
        {
            let mut guard = daemon.inner.lock().unwrap();
            guard.connected = false;
            guard.networks.clear();
            guard.peers = PeerTable::default();
        }
        tracing::info!(seconds = backoff.as_secs(), "reconnecting");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// `UnboundedReceiver` is not `Clone`, and the queue has to outlive any single
/// connection so that messages queued by the CLI survive a reconnect. It lives
/// behind an async mutex, held by whichever connection is currently up.
type ServerQueue = Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<ClientMsg>>>;

#[allow(clippy::too_many_arguments)]
async fn control_session(
    daemon: &Daemon,
    server: &str,
    tun_slot: &Arc<Mutex<Option<Arc<Tun>>>>,
    tun_rx_slot: &Arc<Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>>,
    local_udp_port: u16,
    queue: ServerQueue,
) -> Result<()> {
    let stream = TcpStream::connect(server)
        .await
        .with_context(|| format!("connecting to coordinator at {server}"))?;
    stream.set_nodelay(true).ok();
    let server_ip = stream.peer_addr()?.ip();

    // The route the kernel picks to reach the server is also the address our
    // peers on the same LAN should use.
    if let Ok(local) = stream.local_addr() {
        let mut guard = daemon.inner.lock().unwrap();
        guard.local_endpoint = Some(SocketAddr::new(local.ip(), local_udp_port));
    }

    let (mut reader, mut writer) = stream.into_split();

    let (name, public_key) = {
        let guard = daemon.inner.lock().unwrap();
        (guard.config.name.clone(), guard.config.public_key()?)
    };
    write_frame(
        &mut writer,
        &ClientMsg::Hello { version: PROTOCOL_VERSION, public_key, name },
    )
    .await?;

    let welcome: ServerMsg = read_frame(&mut reader)
        .await?
        .context("coordinator closed before sending welcome")?;
    let ServerMsg::Welcome { version, peer_id, virtual_ip, prefix_len, udp_port, relay_token } =
        welcome
    else {
        if let ServerMsg::Error { message, .. } = welcome {
            bail!("coordinator rejected us: {message}");
        }
        bail!("expected welcome, got something else");
    };
    if version != PROTOCOL_VERSION {
        bail!("coordinator speaks protocol v{version}, this build speaks v{PROTOCOL_VERSION}");
    }

    let identity = Identity {
        peer_id,
        virtual_ip,
        prefix_len,
        relay_token,
        server_udp: SocketAddr::new(server_ip, udp_port),
    };
    tracing::info!(peer_id, %virtual_ip, "connected to coordinator");

    ensure_tun(tun_slot, tun_rx_slot, daemon, &identity)?;

    let networks_to_rejoin = {
        let mut guard = daemon.inner.lock().unwrap();
        guard.identity = Some(identity);
        guard.connected = true;
        guard.last_error = None;
        guard.config.networks.clone()
    };

    // Do this before rejoining anything, so the relay can already deliver by the
    // time the first peer roster arrives and handshakes start.
    daemon.announce_to_relay().await;

    // Rejoin everything we were in before the connection dropped.
    for saved in networks_to_rejoin {
        daemon.inner.lock().unwrap().pending_joins
            .insert(saved.network_id.clone(), saved.password.clone());
        daemon.send_to_server(ClientMsg::JoinNetwork {
            network_id: saved.network_id,
            password: saved.password,
        });
    }

    // Publish our LAN candidate.
    let candidates = daemon.inner.lock().unwrap().candidates_to_publish();
    if !candidates.is_empty() {
        daemon.send_to_server(ClientMsg::UpdateEndpoints { endpoints: candidates });
    }

    // Owns the write half for as long as this connection lives. Aborting the
    // task drops the queue guard, so the next connection can take it over.
    let writer_task = tokio::spawn(async move {
        let mut outbound = queue.lock().await;
        let mut ping = tokio::time::interval(CONTROL_PING);
        ping.tick().await;
        loop {
            tokio::select! {
                msg = outbound.recv() => {
                    let Some(msg) = msg else { return };
                    if write_frame(&mut writer, &msg).await.is_err() {
                        return;
                    }
                }
                _ = ping.tick() => {
                    if write_frame(&mut writer, &ClientMsg::Ping).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    let result = control_read_loop(daemon, &mut reader).await;
    writer_task.abort();
    result
}

async fn control_read_loop(
    daemon: &Daemon,
    reader: &mut tokio::net::tcp::OwnedReadHalf,
) -> Result<()> {
    loop {
        let Some(msg): Option<ServerMsg> = read_frame(reader).await? else {
            return Ok(());
        };
        handle_server_message(daemon, msg).await;
    }
}

async fn handle_server_message(daemon: &Daemon, msg: ServerMsg) {
    match msg {
        ServerMsg::Joined { network, peers } => {
            let mut datagrams = Vec::new();
            {
                let mut guard = daemon.inner.lock().unwrap();
                tracing::info!(
                    network = %network.name,
                    id = %network.network_id,
                    peers = peers.len(),
                    "joined network"
                );
                if let Some(password) = guard.pending_joins.remove(&network.network_id) {
                    guard.config.remember_network(&network.network_id, &password);
                    let path = guard.config_path.clone();
                    if let Err(err) = guard.config.save(&path) {
                        tracing::warn!(%err, "could not save the network to the config");
                    }
                }
                guard.networks.insert(network.network_id.clone(), network.clone());
                for info in peers {
                    let peer_id = info.peer_id;
                    add_peer(&mut guard, info, &mut datagrams);
                    guard.peers.add_to_network(peer_id, &network.network_id);
                }
            }
            let _ = daemon.events.send(Event::Joined {
                network_id: network.network_id,
                name: network.name,
            });
            daemon.flush(datagrams).await;
        }

        ServerMsg::PeerUpdate { network_id, peer } => {
            let mut datagrams = Vec::new();
            {
                let mut guard = daemon.inner.lock().unwrap();
                let peer_id = peer.peer_id;
                add_peer(&mut guard, peer, &mut datagrams);
                guard.peers.add_to_network(peer_id, &network_id);
            }
            daemon.flush(datagrams).await;
        }

        ServerMsg::PeerLeft { network_id, peer_id } => {
            let mut guard = daemon.inner.lock().unwrap();
            // Only drop the peer once we share no networks with them at all.
            if guard.peers.remove_from_network(peer_id, &network_id) {
                tracing::info!(peer_id, "peer left");
            }
        }

        ServerMsg::NetworkCreated { network_id, name } => {
            tracing::info!(%network_id, %name, "network created");
            let _ = daemon.events.send(Event::Created { network_id, name });
        }

        ServerMsg::Left { network_id } => {
            let mut guard = daemon.inner.lock().unwrap();
            guard.networks.remove(&network_id);
            guard.peers.forget_network(&network_id);
            guard.config.forget_network(&network_id);
            let path = guard.config_path.clone();
            let _ = guard.config.save(&path);
            drop(guard);
            let _ = daemon.events.send(Event::Left { network_id });
        }

        ServerMsg::Deleted { network_id } => {
            {
                let mut guard = daemon.inner.lock().unwrap();
                tracing::info!(%network_id, "network deleted");
                guard.networks.remove(&network_id);
                guard.peers.forget_network(&network_id);
                // Stop trying to rejoin something that no longer exists.
                guard.config.forget_network(&network_id);
                let path = guard.config_path.clone();
                let _ = guard.config.save(&path);
            }
            let _ = daemon.events.send(Event::Deleted { network_id });
        }

        ServerMsg::Error { code, message } => {
            tracing::warn!(?code, %message, "coordinator returned an error");
            let _ = daemon.events.send(Event::Failed { message });
        }

        ServerMsg::Welcome { .. } | ServerMsg::Pong => {}
    }
}

/// Adds or refreshes a peer and, if it is our turn to dial, starts a handshake.
fn add_peer(inner: &mut Inner, info: PeerInfo, out: &mut Vec<Datagram>) {
    let peer_id = info.peer_id;
    let Some(identity) = inner.identity.clone() else { return };
    if peer_id == identity.peer_id {
        return;
    }
    let known = inner.peers.get(peer_id).is_some();
    inner.peers.upsert(info);
    if !known {
        tracing::info!(
            peer_id,
            name = %inner.peers.get(peer_id).map(|p| p.info.name.clone()).unwrap_or_default(),
            "peer appeared"
        );
    }
    if should_initiate(identity.peer_id, peer_id) {
        start_handshake(inner, peer_id, out);
    }
    punch(inner, peer_id, out);
}

/// Sends the first Noise message to every candidate address at once, and through
/// the relay as well, so whichever path works wins the race.
fn start_handshake(inner: &mut Inner, peer_id: PeerId, out: &mut Vec<Datagram>) {
    let Some(identity) = inner.identity.clone() else { return };
    let private = inner.config.private_key;
    let Some(peer) = inner.peers.get(peer_id) else { return };
    let remote_public = peer.info.public_key;
    let candidates = peer.candidates.clone();

    let local_index = session::random_index();
    let (initiator, message) = match session::initiate(&private, &remote_public, local_index) {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!(peer_id, %err, "could not start a handshake");
            return;
        }
    };

    let packet = Packet::HandshakeInit { sender_index: local_index, noise: &message }.to_vec();
    for dest in candidates {
        out.push(Datagram { data: packet.clone(), dest });
    }
    out.push(Datagram {
        data: Packet::RelayForward { dst: peer_id, inner: &packet }.to_vec(),
        dest: identity.server_udp,
    });

    if let Some(peer) = inner.peers.get_mut(peer_id) {
        peer.link = Link::Handshaking(Box::new(initiator));
        peer.last_handshake = Some(Instant::now());
    }
    inner.peers.bind_index(peer_id, local_index);
    tracing::debug!(peer_id, "handshake sent");
}

/// Opens an outbound NAT mapping toward every candidate, so the peer's own
/// handshake has somewhere to land. Both sides do this regardless of who dials.
fn punch(inner: &mut Inner, peer_id: PeerId, out: &mut Vec<Datagram>) {
    let Some(identity) = inner.identity.clone() else { return };
    let Some(peer) = inner.peers.get(peer_id) else { return };
    let packet = Packet::Punch { sender: identity.peer_id }.to_vec();
    for dest in &peer.candidates {
        out.push(Datagram { data: packet.clone(), dest: *dest });
    }
}

async fn udp_loop(daemon: Daemon) {
    let mut buf = vec![0u8; MAX_PACKET_SIZE];
    loop {
        let (len, from) = match daemon.socket.recv_from(&mut buf).await {
            Ok(pair) => pair,
            Err(err) => {
                tracing::trace!(%err, "udp recv failed");
                continue;
            }
        };
        let datagrams = handle_datagram(&daemon, &buf[..len], from);
        daemon.flush(datagrams).await;
    }
}

fn handle_datagram(daemon: &Daemon, buf: &[u8], from: SocketAddr) -> Vec<Datagram> {
    let mut out = Vec::new();

    // Unwrap one relay layer, remembering that the path was indirect.
    let (payload, via): (&[u8], Via) = match Packet::decode(buf) {
        Ok(Packet::RelayDeliver { inner, .. }) => (inner, Via::Relay),
        Ok(Packet::ReflectResponse { txid, observed }) => {
            on_reflect(daemon, txid, observed);
            return out;
        }
        Ok(_) => (buf, Via::Direct(from)),
        Err(err) => {
            tracing::trace!(%from, %err, "undecodable datagram");
            return out;
        }
    };

    let Ok(packet) = Packet::decode(payload) else { return out };
    let mut guard = daemon.inner.lock().unwrap();

    match packet {
        Packet::HandshakeInit { sender_index, noise } => {
            on_handshake_init(&mut guard, sender_index, noise, via, &mut out);
        }
        Packet::HandshakeResp { sender_index, receiver_index, noise } => {
            on_handshake_resp(&mut guard, sender_index, receiver_index, noise, via);
        }
        Packet::Data { receiver_index, counter, ciphertext } => {
            if let Some(plaintext) =
                on_data(&mut guard, receiver_index, counter, ciphertext, via, &mut out)
            {
                let _ = daemon.to_tun.send(plaintext);
            }
        }
        // Nothing to do: its only job was to create a NAT mapping on the way out.
        Packet::Punch { sender } => tracing::trace!(sender, %from, "punch received"),
        other => tracing::trace!(?other, "unexpected packet on the client plane"),
    }

    out
}

fn on_reflect(daemon: &Daemon, txid: u64, observed: SocketAddr) {
    let changed = {
        let mut guard = daemon.inner.lock().unwrap();
        // Time the round trip, if this answers the probe still outstanding.
        if let Some((sent_id, sent_at)) = guard.reflect_inflight {
            if sent_id == txid {
                let sample = sent_at.elapsed();
                guard.reflect_inflight = None;
                guard.reflect_samples += 1;
                // The first probe goes out while the NAT mapping is still being
                // created, so it times the setup rather than the path. Seeding
                // the average with it leaves the figure wrong for minutes.
                if guard.reflect_samples > 1 {
                    guard.server_rtt = Some(match guard.server_rtt {
                        None => sample,
                        Some(previous) => previous.mul_f32(0.75) + sample.mul_f32(0.25),
                    });
                }
            }
        }
        if guard.public_endpoint == Some(observed) {
            false
        } else {
            tracing::info!(%observed, "public address discovered");
            guard.public_endpoint = Some(observed);
            true
        }
    };
    if changed {
        let candidates = daemon.inner.lock().unwrap().candidates_to_publish();
        daemon.send_to_server(ClientMsg::UpdateEndpoints { endpoints: candidates });
    }
}

/// Queues a packet back along the path it arrived on.
fn push_to_path(
    out: &mut Vec<Datagram>,
    packet: Vec<u8>,
    peer_id: PeerId,
    via: Via,
    server_udp: SocketAddr,
) {
    match via {
        Via::Direct(addr) => out.push(Datagram { data: packet, dest: addr }),
        Via::Relay => out.push(Datagram {
            data: Packet::RelayForward { dst: peer_id, inner: &packet }.to_vec(),
            dest: server_udp,
        }),
    }
}

fn on_handshake_init(
    inner: &mut Inner,
    sender_index: u32,
    noise: &[u8],
    via: Via,
    out: &mut Vec<Datagram>,
) {
    let private = inner.config.private_key;
    let responder = match session::respond(&private, noise) {
        Ok(responder) => responder,
        Err(err) => {
            tracing::debug!(%err, "rejected a handshake we could not read");
            return;
        }
    };

    // Authorisation: the key must belong to somebody the coordinator told us
    // shares a network with us. Anyone else is dropped without a reply.
    let Some(peer_id) = inner.peers.by_public_key(&responder.remote_public) else {
        tracing::debug!("handshake from an unknown key, dropping");
        return;
    };

    let Some(identity) = inner.identity.clone() else { return };

    // Same initiation arriving over another path: replay the answer we already
    // gave, so both sides stay on the one session rather than building a new one
    // per path. A genuinely new initiation carries a different index and falls
    // through to the code below.
    if let Some(peer) = inner.peers.get(peer_id) {
        if let Some((their_index, _, response)) = &peer.last_response {
            if *their_index == sender_index && peer.link.is_up() {
                tracing::trace!(peer_id, "replaying the handshake answer on a new path");
                push_to_path(out, response.clone(), peer_id, via, identity.server_udp);
                return;
            }
        }
    }

    let local_index = session::random_index();
    let (session, reply) = match responder.finish(local_index, sender_index) {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!(peer_id, %err, "could not complete a handshake");
            return;
        }
    };

    let packet =
        Packet::HandshakeResp { sender_index: local_index, receiver_index: sender_index, noise: &reply }
            .to_vec();

    push_to_path(out, packet.clone(), peer_id, via, identity.server_udp);

    if let Some(peer) = inner.peers.get_mut(peer_id) {
        peer.link = Link::Up(Box::new(session));
        peer.last_response = Some((sender_index, local_index, packet));
        peer.last_rx = Some(Instant::now());
        match via {
            Via::Direct(addr) => {
                peer.confirmed = Some(addr);
                tracing::info!(peer_id, %addr, "tunnel up (direct)");
            }
            Via::Relay => {
                peer.relay_seen = true;
                tracing::info!(peer_id, "tunnel up (relayed)");
            }
        }
    }
    inner.peers.bind_index(peer_id, local_index);
}

fn on_handshake_resp(
    inner: &mut Inner,
    sender_index: u32,
    receiver_index: u32,
    noise: &[u8],
    via: Via,
) {
    let Some(peer_id) = inner.peers.by_session_index(receiver_index) else {
        tracing::trace!(receiver_index, "handshake response for an unknown session");
        return;
    };
    let Some(peer) = inner.peers.get_mut(peer_id) else { return };

    // Check before taking. An initiation is raced down several paths, so the
    // answers come back more than once; taking the link out unconditionally
    // would tear down the session the first answer just established.
    if !matches!(peer.link, Link::Handshaking(_)) {
        tracing::trace!(peer_id, "ignoring a duplicate handshake response");
        return;
    }
    let Link::Handshaking(initiator) = std::mem::replace(&mut peer.link, Link::Idle) else {
        unreachable!("just checked that the link is handshaking")
    };

    match initiator.finish(noise, sender_index) {
        Ok(session) => {
            peer.link = Link::Up(Box::new(session));
            peer.last_rx = Some(Instant::now());
            match via {
                Via::Direct(addr) => {
                    peer.confirmed = Some(addr);
                    tracing::info!(peer_id, %addr, "tunnel up (direct)");
                }
                Via::Relay => {
                    peer.relay_seen = true;
                    tracing::info!(peer_id, "tunnel up (relayed)");
                }
            }
        }
        Err(err) => tracing::debug!(peer_id, %err, "handshake response rejected"),
    }
}

/// Returns the decrypted IP packet, if this was one rather than a keepalive.
fn on_data(
    inner: &mut Inner,
    receiver_index: u32,
    counter: u64,
    ciphertext: &[u8],
    via: Via,
    out: &mut Vec<Datagram>,
) -> Option<Vec<u8>> {
    let peer_id = inner.peers.by_session_index(receiver_index)?;
    let peer = inner.peers.get_mut(peer_id)?;
    let session = peer.link.session_mut()?;

    let mut plaintext = Vec::new();
    let len = match session.decrypt(counter, ciphertext, &mut plaintext) {
        Ok(len) => len,
        Err(err) => {
            tracing::trace!(peer_id, %err, "dropping a packet");
            return None;
        }
    };

    peer.last_rx = Some(Instant::now());
    peer.rx_bytes += len as u64;

    // Only now, with the packet authenticated, is it safe to believe where it
    // came from. Trusting the source address of anything unverified would let a
    // spoofer redirect a peer's traffic.
    match via {
        Via::Direct(addr) => {
            if peer.confirmed != Some(addr) {
                tracing::info!(peer_id, %addr, "direct path confirmed");
                peer.confirmed = Some(addr);
            }
        }
        Via::Relay => peer.relay_seen = true,
    }

    // An empty payload is a bare keepalive.
    if len == 0 {
        return None;
    }
    plaintext.truncate(len);

    // Our own in-tunnel control messages, distinguishable from IP because no IP
    // packet can begin with a zero byte.
    if plaintext[0] == hamradnet_proto::CONTROL_PREFIX {
        handle_control(inner, peer_id, &plaintext, out);
        return None;
    }

    Some(plaintext)
}

/// Builds a control frame: `[0x00][kind][id: u64 big-endian]`.
fn control_frame(kind: u8, id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    out.push(hamradnet_proto::CONTROL_PREFIX);
    out.push(kind);
    out.extend_from_slice(&id.to_be_bytes());
    out
}

/// Answers a latency probe, or records the answer to one of ours.
fn handle_control(inner: &mut Inner, peer_id: PeerId, frame: &[u8], out: &mut Vec<Datagram>) {
    if frame.len() < 10 {
        return;
    }
    let mut id_bytes = [0u8; 8];
    id_bytes.copy_from_slice(&frame[2..10]);
    let id = u64::from_be_bytes(id_bytes);

    match frame[1] {
        hamradnet_proto::CONTROL_PING => {
            // Echo it straight back; the sender times the round trip.
            if let Some(datagram) =
                encrypt_for(inner, peer_id, &control_frame(hamradnet_proto::CONTROL_PONG, id))
            {
                out.push(datagram);
            }
        }
        hamradnet_proto::CONTROL_PONG => {
            let Some(peer) = inner.peers.get_mut(peer_id) else { return };
            // Only the probe still outstanding counts, so a late duplicate
            // cannot report an inflated time.
            if let Some((sent_id, sent_at)) = peer.ping_inflight {
                if sent_id == id {
                    peer.record_rtt(sent_at.elapsed());
                    peer.ping_inflight = None;
                }
            }
        }
        other => tracing::trace!(peer_id, other, "unknown control message"),
    }
}

/// Encrypts one payload for a peer and addresses it to the peer's current path.
fn encrypt_for(inner: &mut Inner, peer_id: PeerId, payload: &[u8]) -> Option<Datagram> {
    let server_udp = inner.identity.as_ref()?.server_udp;
    let peer = inner.peers.get_mut(peer_id)?;
    let path = peer.path();
    let remote_index = peer.link.session_mut()?.remote_index;
    let session = peer.link.session_mut()?;

    let mut ciphertext = Vec::new();
    let counter = session.encrypt(payload, &mut ciphertext).ok()?;
    let packet =
        Packet::Data { receiver_index: remote_index, counter, ciphertext: &ciphertext }.to_vec();

    peer.last_tx = Some(Instant::now());
    peer.tx_bytes += payload.len() as u64;

    Some(match path {
        SendPath::Direct(addr) => Datagram { data: packet, dest: addr },
        SendPath::Relay => Datagram {
            data: Packet::RelayForward { dst: peer_id, inner: &packet }.to_vec(),
            dest: server_udp,
        },
    })
}

/// Routes one packet that came out of the tun device.
pub fn route_outbound(inner: &mut Inner, packet: &[u8], out: &mut Vec<Datagram>) {
    let Some(dst) = ip::destination(packet) else { return };

    if ip::is_fanout(dst) {
        // Copy to every peer, the way a switch floods a broadcast.
        for peer_id in inner.peers.ids() {
            if let Some(datagram) = encrypt_for(inner, peer_id, packet) {
                out.push(datagram);
            }
        }
        return;
    }

    let Some(peer_id) = inner.peers.by_virtual_ip(dst) else {
        tracing::trace!(%dst, "no peer for destination");
        return;
    };
    if let Some(datagram) = encrypt_for(inner, peer_id, packet) {
        out.push(datagram);
    }
}

async fn tun_router(daemon: Daemon, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(packet) = rx.recv().await {
        let mut out = Vec::new();
        {
            let mut guard = daemon.inner.lock().unwrap();
            route_outbound(&mut guard, &packet, &mut out);
        }
        daemon.flush(out).await;
    }
}

/// Drains decrypted packets onto the interface from a thread of its own.
///
/// This used to hand every single packet to `spawn_blocking`, which meant a
/// thread-pool round trip per packet - a large and needless share of the latency
/// on the receive path. Writes block, so they belong on a dedicated thread.
/// Writes every packet it is given, and keeps going when one cannot be written.
///
/// One bad packet must never take the receive path down with it. This loop used
/// to return on the first error, which killed the writer for the lifetime of the
/// process: handshakes and latency probes carried on working, because neither
/// touches the interface, so the tunnel looked perfectly healthy while no
/// traffic was delivered at all. A single oversized packet was enough to do it.
fn write_loop(
    mut next: impl FnMut() -> Option<Vec<u8>>,
    mut write: impl FnMut(&[u8]) -> Result<()>,
) {
    let mut failures: u64 = 0;
    while let Some(packet) = next() {
        if let Err(err) = write(&packet) {
            failures += 1;
            // Log the first, then at a decreasing rate, so a pathological peer
            // cannot flood the log.
            if failures.is_power_of_two() {
                tracing::warn!(
                    %err,
                    failures,
                    bytes = packet.len(),
                    "dropping a packet the interface refused"
                );
            }
        }
    }
}

fn spawn_tun_writer(tun: Arc<Tun>, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) -> Result<()> {
    std::thread::Builder::new()
        .name("tun-writer".into())
        .spawn(move || {
            write_loop(|| rx.blocking_recv(), |packet| tun.write(packet));
            tracing::info!("tun writer stopped: the device was closed");
        })
        .context("spawning the tun writer thread")?;
    Ok(())
}

/// Periodic maintenance: handshakes, keepalives, path probing, relay refresh.
async fn tick_loop(daemon: Daemon) {
    let mut ticker = tokio::time::interval(TICK);
    let mut since_refresh = Duration::ZERO;
    loop {
        ticker.tick().await;
        since_refresh += TICK;
        let refresh = since_refresh >= SERVER_REFRESH;
        if refresh {
            since_refresh = Duration::ZERO;
        }

        let mut out = Vec::new();
        {
            let mut guard = daemon.inner.lock().unwrap();
            tick(&mut guard, refresh, &mut out);
        }
        daemon.flush(out).await;
    }
}

fn tick(inner: &mut Inner, refresh_server: bool, out: &mut Vec<Datagram>) {
    let Some(identity) = inner.identity.clone() else { return };
    let now = Instant::now();

    if refresh_server {
        // Re-register so the relay keeps our mapping, re-check our public
        // address in case the NAT rebound it, and time the round trip.
        let (datagrams, txid) = server_refresh_datagrams(&identity);
        inner.reflect_inflight = Some((txid, Instant::now()));
        out.extend(datagrams);
    }

    for peer_id in inner.peers.ids() {
        let Some(peer) = inner.peers.get(peer_id) else { continue };
        let (stale, needs_handshake, needs_keepalive, is_direct) =
            (peer.is_stale(now), peer.needs_handshake(now), peer.needs_keepalive(now), peer.is_direct());

        if stale {
            tracing::info!(peer_id, "peer went quiet, restarting the tunnel");
            if let Some(peer) = inner.peers.get_mut(peer_id) {
                peer.link = Link::Idle;
                peer.confirmed = None;
                peer.relay_seen = false;
                // Drop the cached answer too, so a fresh initiation is treated
                // as new rather than replayed against a session that is gone.
                peer.last_response = None;
            }
            if should_initiate(identity.peer_id, peer_id) {
                start_handshake(inner, peer_id, out);
            }
            punch(inner, peer_id, out);
            continue;
        }

        if needs_handshake && should_initiate(identity.peer_id, peer_id) {
            start_handshake(inner, peer_id, out);
        }
        if needs_handshake {
            punch(inner, peer_id, out);
        }

        if needs_keepalive {
            // An authenticated probe: holds the NAT mapping open and measures
            // the round trip at the same time.
            let id: u64 = rand::thread_rng().gen();
            if let Some(datagram) =
                encrypt_for(inner, peer_id, &control_frame(hamradnet_proto::CONTROL_PING, id))
            {
                if let Some(peer) = inner.peers.get_mut(peer_id) {
                    peer.ping_inflight = Some((id, Instant::now()));
                }
                out.push(datagram);
            }
            if !is_direct {
                probe_candidates(inner, peer_id, out);
            }
        }
    }
}

/// Sends an authenticated keepalive straight at each candidate address. If one
/// arrives, the peer confirms our address and replies directly, and the pair
/// stops paying the relay's latency.
fn probe_candidates(inner: &mut Inner, peer_id: PeerId, out: &mut Vec<Datagram>) {
    let Some(peer) = inner.peers.get(peer_id) else { return };
    if !peer.link.is_up() {
        return;
    }
    let candidates = peer.candidates.clone();
    let remote_index = match &peer.link {
        Link::Up(session) => session.remote_index,
        _ => return,
    };

    for dest in candidates {
        let Some(peer) = inner.peers.get_mut(peer_id) else { return };
        let Some(session) = peer.link.session_mut() else { return };
        let mut ciphertext = Vec::new();
        let Ok(counter) = session.encrypt(&[], &mut ciphertext) else { return };
        out.push(Datagram {
            data: Packet::Data { receiver_index: remote_index, counter, ciphertext: &ciphertext }
                .to_vec(),
            dest,
        });
    }
}

fn ensure_tun(
    tun_slot: &Arc<Mutex<Option<Arc<Tun>>>>,
    tun_rx_slot: &Arc<Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>>,
    daemon: &Daemon,
    identity: &Identity,
) -> Result<()> {
    let existing = tun_slot.lock().unwrap().clone();
    match existing {
        Some(tun) => {
            // A reconnect gets a fresh address; move the interface to it.
            tun.set_address(identity.virtual_ip, identity.prefix_len)
                .context("reassigning the tunnel address")?;
            tracing::info!(address = %identity.virtual_ip, "tunnel address updated");
        }
        None => {
            let config = TunConfig {
                name: INTERFACE_NAME.to_string(),
                address: identity.virtual_ip,
                prefix_len: identity.prefix_len,
                mtu: TUN_MTU,
            };
            let tun = Arc::new(Tun::open(&config).context("creating the tunnel interface")?);
            tracing::info!(
                interface = INTERFACE_NAME,
                address = %identity.virtual_ip,
                "tunnel interface up"
            );
            *tun_slot.lock().unwrap() = Some(Arc::clone(&tun));

            // Writes block too, so they get a thread rather than the runtime.
            if let Some(rx) = tun_rx_slot.lock().unwrap().take() {
                spawn_tun_writer(Arc::clone(&tun), rx)?;
            }

            // Reads block, so the device gets its own thread.
            let (tx, rx) = mpsc::unbounded_channel();
            #[cfg(target_os = "linux")]
            let broadcast_tx = tx.clone();
            let reader_tun = Arc::clone(&tun);
            std::thread::Builder::new()
                .name("tun-reader".into())
                .spawn(move || {
                    let mut buf = vec![0u8; MAX_PACKET_SIZE];
                    // A run of failures means the device has gone; a single one
                    // means a packet we could not take. Giving up on the first
                    // error would silently stop all outbound traffic while the
                    // tunnel still looked healthy.
                    let mut consecutive_failures = 0u32;
                    loop {
                        match reader_tun.read(&mut buf) {
                            Ok(0) => continue,
                            Ok(len) => {
                                consecutive_failures = 0;
                                if tx.send(buf[..len].to_vec()).is_err() {
                                    return;
                                }
                            }
                            Err(err) => {
                                consecutive_failures += 1;
                                if consecutive_failures >= READ_FAILURE_LIMIT {
                                    tracing::info!(
                                        %err,
                                        "tun reader stopped: the device is gone"
                                    );
                                    return;
                                }
                                tracing::warn!(%err, "skipping an unreadable packet");
                            }
                        }
                    }
                })
                .context("spawning the tun reader thread")?;

            tokio::spawn(tun_router(daemon.clone(), rx));

            // Copies of games' LAN broadcasts join the tunnel's own outbound
            // packets; see broadcast.rs for why they need fetching.
            #[cfg(target_os = "linux")]
            {
                let source = daemon.clone();
                let copied = crate::broadcast::spawn(
                    INTERFACE_NAME,
                    move || source.inner.lock().unwrap().identity.as_ref().map(|i| i.virtual_ip),
                    move |packet| broadcast_tx.send(packet).is_ok(),
                );
                match copied {
                    Ok(()) => tracing::info!("copying LAN game broadcasts into the tunnel"),
                    Err(err) => tracing::warn!(
                        %err,
                        "cannot copy LAN broadcasts into the tunnel; games that announce \
                         themselves to 255.255.255.255 will not be seen by the other players"
                    ),
                }
            }
        }
    }
    Ok(())
}

/// Snapshot for `hamradnet status`.
pub fn status(inner: &Inner) -> ipc::Status {
    let identity = inner.identity.as_ref();
    let direct_count = inner.peers.iter().filter(|(_, p)| p.is_direct()).count();
    ipc::Status {
        connected: inner.connected,
        server: inner.config.server.clone(),
        name: inner.config.name.clone(),
        peer_id: identity.map(|i| i.peer_id),
        virtual_ip: identity.map(|i| i.virtual_ip),
        public_endpoint: inner.public_endpoint.map(|a| a.to_string()),
        networks: inner
            .networks
            .values()
            .map(|n| ipc::NetworkReport {
                network_id: n.network_id.clone(),
                name: n.name.clone(),
            })
            .collect(),
        peer_count: inner.peers.len(),
        direct_count,
        server_rtt_ms: inner.server_rtt.map(|d| d.as_secs_f32() * 1000.0),
        total_rx: inner.peers.iter().map(|(_, p)| p.rx_bytes).sum(),
        total_tx: inner.peers.iter().map(|(_, p)| p.tx_bytes).sum(),
        last_error: inner.last_error.clone(),
    }
}

/// Snapshot for `hamradnet peers`.
pub fn peer_reports(inner: &Inner) -> Vec<ipc::PeerReport> {
    let now = Instant::now();
    let mut reports: Vec<ipc::PeerReport> = inner
        .peers
        .iter()
        .map(|(peer_id, peer)| ipc::PeerReport {
            peer_id: *peer_id,
            name: peer.info.name.clone(),
            virtual_ip: peer.info.virtual_ip,
            link: match peer.link {
                Link::Up(_) => "up",
                Link::Handshaking(_) => "connecting",
                Link::Idle => "idle",
            }
            .to_string(),
            path: if !peer.link.is_up() {
                "-".to_string()
            } else if peer.is_direct() {
                "direct".to_string()
            } else {
                "relay".to_string()
            },
            endpoint: peer.confirmed.map(|a| a.to_string()),
            rx_bytes: peer.rx_bytes,
            tx_bytes: peer.tx_bytes,
            last_seen_secs: peer.last_rx.map(|at| now.duration_since(at).as_secs()),
            rtt_ms: peer.rtt.map(|d| d.as_secs_f32() * 1000.0),
            networks: peer.networks.iter().cloned().collect(),
        })
        .collect();
    reports.sort_by_key(|r| r.virtual_ip);
    reports
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a failing write used to end the loop, so everything after the
    /// first bad packet was silently never delivered. The tunnel still reported
    /// itself healthy, because pings do not go through the interface.
    #[test]
    fn a_failed_write_does_not_stop_the_ones_after_it() {
        let packets: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 4]).collect();
        let mut queue = packets.clone().into_iter();
        let mut attempted: Vec<Vec<u8>> = Vec::new();

        write_loop(
            || queue.next(),
            |packet| {
                attempted.push(packet.to_vec());
                // The middle one is rejected, as an oversized packet would be.
                if packet[0] == 2 {
                    bail!("interface refused this packet");
                }
                Ok(())
            },
        );

        assert_eq!(attempted, packets, "every packet must still be attempted");
    }

    #[test]
    fn the_writer_survives_every_write_failing() {
        let mut queue = (0..3u8).map(|i| vec![i]).collect::<Vec<_>>().into_iter();
        let mut attempts = 0;
        write_loop(
            || queue.next(),
            |_| {
                attempts += 1;
                bail!("always fails")
            },
        );
        assert_eq!(attempts, 3, "the loop must drain the queue regardless");
    }

    #[test]
    fn the_writer_stops_when_the_queue_closes() {
        let mut queue = std::iter::empty::<Vec<u8>>();
        let mut attempts = 0;
        write_loop(
            || queue.next(),
            |_| {
                attempts += 1;
                Ok(())
            },
        );
        assert_eq!(attempts, 0);
    }
}
