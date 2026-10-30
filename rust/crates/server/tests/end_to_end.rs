//! End-to-end tests against the real server binary.
//!
//! These drive the coordinator the way a client does - over TCP and UDP, with
//! no access to its internals - and carry out a genuine Noise handshake between
//! two independent "clients" through the relay. The Noise code here is written
//! directly against `snow` rather than reusing the client crate, so the wire
//! protocol is validated by a second implementation.

use std::io::{BufRead, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use hamradnet_proto::{
    wire::Packet, ClientMsg, ErrorCode, NetworkInfo, PeerInfo, ServerMsg, PROTOCOL_VERSION,
};

const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// Kills the server when the test ends, however it ends.
struct ServerProcess {
    child: Child,
    stdout: Option<std::process::ChildStdout>,
    control: SocketAddr,
    udp: SocketAddr,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a server on OS-chosen ports and reads back the ones it got.
///
/// Picking a free port in the test and handing it over would race: another test
/// running in parallel can take the port in the gap between the probe and the
/// server binding it. Letting the server bind port 0 and report the result
/// removes the gap entirely, so these tests are safe to run concurrently.
/// The coordinator under test.
///
/// Defaults to the Rust binary, but `HAMRADNET_SERVER_BIN` points it at any
/// other implementation. The suite only ever speaks the wire protocol, so it
/// doubles as a conformance test: the Go coordinator has to pass it unchanged.
fn server_binary() -> String {
    std::env::var("HAMRADNET_SERVER_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_hamradnet-server").to_string())
}

fn start_server() -> ServerProcess {
    let mut child = Command::new(server_binary())
        .args([
            "--bind",
            "127.0.0.1",
            "--control-port",
            "0",
            "--udp-port",
            "0",
            "--no-store",
            // Remove networks the instant they empty, so the behaviour is
            // observable without the test waiting out a grace period.
            "--empty-grace",
            "0",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("starting the server binary");

    // Wrap the child straight away so that every path out of this function,
    // including the panics below, still reaps the process.
    let mut server = ServerProcess {
        stdout: child.stdout.take(),
        child,
        control: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        udp: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
    };

    let stdout = server.stdout.take().expect("piped stdout");
    let mut line = String::new();
    std::io::BufReader::new(stdout)
        .read_line(&mut line)
        .expect("reading the server's listening line");

    let (control, udp) = parse_listening_line(&line)
        .unwrap_or_else(|| panic!("could not parse the server's listening line: {line:?}"));
    server.control = control;
    server.udp = udp;

    // The line is printed after both sockets are bound, so the server is ready.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&control, Duration::from_millis(200)).is_ok() {
            return server;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("server did not accept connections on {control}");
}

/// Parses `listening control=127.0.0.1:1234 udp=127.0.0.1:5678`.
fn parse_listening_line(line: &str) -> Option<(SocketAddr, SocketAddr)> {
    let mut control = None;
    let mut udp = None;
    for field in line.split_whitespace() {
        if let Some(value) = field.strip_prefix("control=") {
            control = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("udp=") {
            udp = value.parse().ok();
        }
    }
    Some((control?, udp?))
}

#[test]
fn the_listening_line_is_machine_readable() {
    let (control, udp) =
        parse_listening_line("listening control=127.0.0.1:1234 udp=127.0.0.1:5678\n").unwrap();
    assert_eq!(control.port(), 1234);
    assert_eq!(udp.port(), 5678);
    assert!(parse_listening_line("something else entirely").is_none());
}

/// A test client's control connection.
struct Client {
    stream: TcpStream,
    pub peer_id: u32,
    pub virtual_ip: Ipv4Addr,
    pub relay_token: [u8; hamradnet_proto::wire::RELAY_TOKEN_LEN],
    pub private_key: [u8; 32],
    pub public_key: [u8; 32],
}

fn send_msg(stream: &mut TcpStream, msg: &ClientMsg) {
    let body = serde_json::to_vec(msg).unwrap();
    stream.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
    stream.write_all(&body).unwrap();
    stream.flush().unwrap();
}

fn recv_msg(stream: &mut TcpStream) -> ServerMsg {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes).unwrap();
    let len = u32::from_be_bytes(len_bytes) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).unwrap();
    serde_json::from_slice(&buf).unwrap()
}

fn keypair() -> ([u8; 32], [u8; 32]) {
    let keys = snow::Builder::new(NOISE_PARAMS.parse().unwrap())
        .generate_keypair()
        .unwrap();
    (
        keys.private.try_into().unwrap(),
        keys.public.try_into().unwrap(),
    )
}

impl Client {
    fn connect(server: &ServerProcess, name: &str) -> Self {
        let mut stream = TcpStream::connect(server.control).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (private_key, public_key) = keypair();

        send_msg(
            &mut stream,
            &ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                public_key,
                name: name.to_string(),
            },
        );

        match recv_msg(&mut stream) {
            ServerMsg::Welcome { peer_id, virtual_ip, relay_token, version, .. } => {
                assert_eq!(version, PROTOCOL_VERSION);
                Client { stream, peer_id, virtual_ip, relay_token, private_key, public_key }
            }
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    fn create_network(&mut self, name: &str, password: &str) -> String {
        send_msg(
            &mut self.stream,
            &ClientMsg::CreateNetwork {
                name: name.to_string(),
                password: password.to_string(),
            },
        );
        match recv_msg(&mut self.stream) {
            ServerMsg::NetworkCreated { network_id, .. } => network_id,
            other => panic!("expected network_created, got {other:?}"),
        }
    }

    fn delete(&mut self, network_id: &str) -> Result<String, ErrorCode> {
        send_msg(
            &mut self.stream,
            &ClientMsg::DeleteNetwork { network_id: network_id.to_string() },
        );
        match recv_msg(&mut self.stream) {
            ServerMsg::Deleted { network_id } => Ok(network_id),
            ServerMsg::Error { code, .. } => Err(code),
            other => panic!("expected deleted, got {other:?}"),
        }
    }

    fn join(&mut self, network_id: &str, password: &str) -> Result<(NetworkInfo, Vec<PeerInfo>), ErrorCode> {
        send_msg(
            &mut self.stream,
            &ClientMsg::JoinNetwork {
                network_id: network_id.to_string(),
                password: password.to_string(),
            },
        );
        match recv_msg(&mut self.stream) {
            ServerMsg::Joined { network, peers } => Ok((network, peers)),
            ServerMsg::Error { code, .. } => Err(code),
            other => panic!("expected joined, got {other:?}"),
        }
    }

    /// Binds a UDP socket and registers it with the relay.
    fn open_udp(&self, server: &ServerProcess) -> UdpSocket {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let register = Packet::RelayRegister {
            peer_id: self.peer_id,
            token: self.relay_token,
        }
        .to_vec();
        socket.send_to(&register, server.udp).unwrap();
        socket
    }
}

#[test]
fn a_peer_connecting_is_given_an_address_in_the_virtual_subnet() {
    let server = start_server();
    let client = Client::connect(&server, "alice");
    let octets = client.virtual_ip.octets();
    assert_eq!([octets[0], octets[1]], [10, 66], "address must be in 10.66/16");
    assert!(client.peer_id > 0);
}

#[test]
fn two_peers_in_a_network_learn_about_each_other() {
    let server = start_server();
    let mut alice = Client::connect(&server, "alice");
    let mut bob = Client::connect(&server, "bob");

    let network_id = alice.create_network("game night", "hunter2");
    let (_, roster) = alice.join(&network_id, "hunter2").unwrap();
    assert!(roster.is_empty(), "alice is first in, so she sees nobody");

    let (network, roster) = bob.join(&network_id, "hunter2").unwrap();
    assert_eq!(network.name, "game night");
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0].peer_id, alice.peer_id);
    assert_eq!(roster[0].name, "alice");
    assert_eq!(roster[0].public_key, alice.public_key);
    assert_ne!(alice.virtual_ip, bob.virtual_ip, "addresses must be unique");

    // Alice is told about Bob without having asked.
    match recv_msg(&mut alice.stream) {
        ServerMsg::PeerUpdate { peer, .. } => {
            assert_eq!(peer.peer_id, bob.peer_id);
            assert_eq!(peer.name, "bob");
        }
        other => panic!("expected a peer update, got {other:?}"),
    }
}

#[test]
fn the_wrong_password_does_not_get_you_in() {
    let server = start_server();
    let mut alice = Client::connect(&server, "alice");
    let mut mallory = Client::connect(&server, "mallory");

    let network_id = alice.create_network("private", "correct horse");
    assert_eq!(
        mallory.join(&network_id, "guess").unwrap_err(),
        ErrorCode::BadPassword
    );
    assert_eq!(
        mallory.join("HAM-0000-0000", "whatever").unwrap_err(),
        ErrorCode::NoSuchNetwork
    );
}

#[test]
fn a_peer_leaving_is_announced() {
    let server = start_server();
    let mut alice = Client::connect(&server, "alice");
    let mut bob = Client::connect(&server, "bob");

    let network_id = alice.create_network("game night", "pw");
    alice.join(&network_id, "pw").unwrap();
    bob.join(&network_id, "pw").unwrap();
    // Consume the join notification.
    recv_msg(&mut alice.stream);

    let bob_id = bob.peer_id;
    drop(bob);

    match recv_msg(&mut alice.stream) {
        ServerMsg::PeerLeft { peer_id, .. } => assert_eq!(peer_id, bob_id),
        other => panic!("expected peer_left, got {other:?}"),
    }
}

#[test]
fn the_server_reports_the_address_it_sees() {
    let server = start_server();
    let client = Client::connect(&server, "alice");
    let socket = client.open_udp(&server);

    socket
        .send_to(&Packet::ReflectRequest { txid: 0xABCD }.to_vec(), server.udp)
        .unwrap();

    let mut buf = [0u8; 1500];
    let (len, _) = socket.recv_from(&mut buf).unwrap();
    match Packet::decode(&buf[..len]).unwrap() {
        Packet::ReflectResponse { txid, observed } => {
            assert_eq!(txid, 0xABCD);
            assert_eq!(observed, socket.local_addr().unwrap());
        }
        other => panic!("expected a reflect response, got {other:?}"),
    }
}

/// The core of the whole system: two clients complete a Noise handshake and
/// exchange an encrypted packet, with the server relaying and never holding a key.
#[test]
fn two_peers_tunnel_encrypted_traffic_through_the_relay() {
    let server = start_server();
    let mut alice = Client::connect(&server, "alice");
    let mut bob = Client::connect(&server, "bob");

    let network_id = alice.create_network("game night", "pw");
    alice.join(&network_id, "pw").unwrap();
    bob.join(&network_id, "pw").unwrap();
    recv_msg(&mut alice.stream); // Bob's arrival.

    let alice_udp = alice.open_udp(&server);
    let bob_udp = bob.open_udp(&server);

    // --- Alice initiates, addressing Bob's published static key. ---
    let mut alice_hs = snow::Builder::new(NOISE_PARAMS.parse().unwrap())
        .local_private_key(&alice.private_key)
        .remote_public_key(&bob.public_key)
        .build_initiator()
        .unwrap();
    let mut msg1 = vec![0u8; 1024];
    let len = alice_hs.write_message(&[], &mut msg1).unwrap();
    msg1.truncate(len);

    let init = Packet::HandshakeInit { sender_index: 1001, noise: &msg1 }.to_vec();
    alice_udp
        .send_to(
            &Packet::RelayForward { dst: bob.peer_id, inner: &init }.to_vec(),
            server.udp,
        )
        .unwrap();

    // --- Bob receives it through the relay and answers. ---
    let mut buf = [0u8; 1500];
    let (len, from) = bob_udp.recv_from(&mut buf).unwrap();
    assert_eq!(from, server.udp, "the relay is the apparent source");

    let (alice_index, noise) = match Packet::decode(&buf[..len]).unwrap() {
        Packet::RelayDeliver { src, inner } => {
            assert_eq!(src, alice.peer_id, "the relay identifies the sender");
            match Packet::decode(inner).unwrap() {
                Packet::HandshakeInit { sender_index, noise } => (sender_index, noise.to_vec()),
                other => panic!("expected a handshake init, got {other:?}"),
            }
        }
        other => panic!("expected a relay delivery, got {other:?}"),
    };
    assert_eq!(alice_index, 1001);

    let mut bob_hs = snow::Builder::new(NOISE_PARAMS.parse().unwrap())
        .local_private_key(&bob.private_key)
        .build_responder()
        .unwrap();
    let mut scratch = vec![0u8; 1024];
    bob_hs.read_message(&noise, &mut scratch).unwrap();
    // Bob can tell who called before answering.
    assert_eq!(bob_hs.get_remote_static().unwrap(), alice.public_key);

    let mut msg2 = vec![0u8; 1024];
    let len = bob_hs.write_message(&[], &mut msg2).unwrap();
    msg2.truncate(len);
    let bob_transport = bob_hs.into_stateless_transport_mode().unwrap();

    let resp =
        Packet::HandshakeResp { sender_index: 2002, receiver_index: alice_index, noise: &msg2 }
            .to_vec();
    bob_udp
        .send_to(
            &Packet::RelayForward { dst: alice.peer_id, inner: &resp }.to_vec(),
            server.udp,
        )
        .unwrap();

    // --- Alice completes the handshake. ---
    let (len, _) = alice_udp.recv_from(&mut buf).unwrap();
    let (bob_index, noise) = match Packet::decode(&buf[..len]).unwrap() {
        Packet::RelayDeliver { src, inner } => {
            assert_eq!(src, bob.peer_id);
            match Packet::decode(inner).unwrap() {
                Packet::HandshakeResp { sender_index, receiver_index, noise } => {
                    assert_eq!(receiver_index, 1001, "must address Alice's session");
                    (sender_index, noise.to_vec())
                }
                other => panic!("expected a handshake response, got {other:?}"),
            }
        }
        other => panic!("expected a relay delivery, got {other:?}"),
    };
    alice_hs.read_message(&noise, &mut scratch).unwrap();
    let alice_transport = alice_hs.into_stateless_transport_mode().unwrap();

    // --- An encrypted payload crosses the relay. ---
    let secret = b"this is a game packet";
    let mut ciphertext = vec![0u8; secret.len() + 16];
    let written = alice_transport.write_message(0, secret, &mut ciphertext).unwrap();
    ciphertext.truncate(written);

    // The relay carries the payload without being able to read it.
    assert!(
        !ciphertext.windows(4).any(|w| w == b"game"),
        "payload must not appear in the clear"
    );

    let data = Packet::Data { receiver_index: bob_index, counter: 0, ciphertext: &ciphertext }
        .to_vec();
    alice_udp
        .send_to(
            &Packet::RelayForward { dst: bob.peer_id, inner: &data }.to_vec(),
            server.udp,
        )
        .unwrap();

    let (len, _) = bob_udp.recv_from(&mut buf).unwrap();
    let (counter, ciphertext) = match Packet::decode(&buf[..len]).unwrap() {
        Packet::RelayDeliver { inner, .. } => match Packet::decode(inner).unwrap() {
            Packet::Data { receiver_index, counter, ciphertext } => {
                assert_eq!(receiver_index, 2002, "must address Bob's session");
                (counter, ciphertext.to_vec())
            }
            other => panic!("expected data, got {other:?}"),
        },
        other => panic!("expected a relay delivery, got {other:?}"),
    };

    let mut plaintext = vec![0u8; ciphertext.len()];
    let len = bob_transport.read_message(counter, &ciphertext, &mut plaintext).unwrap();
    assert_eq!(&plaintext[..len], secret, "Bob recovers exactly what Alice sent");
}

/// The relay must not be usable to reach someone you share no network with.
#[test]
fn the_relay_refuses_to_forward_between_strangers() {
    let server = start_server();
    let mut alice = Client::connect(&server, "alice");
    let mut bob = Client::connect(&server, "bob");
    let stranger = Client::connect(&server, "stranger");

    let network_id = alice.create_network("game night", "pw");
    alice.join(&network_id, "pw").unwrap();
    bob.join(&network_id, "pw").unwrap();
    recv_msg(&mut alice.stream);

    let stranger_udp = stranger.open_udp(&server);
    let bob_udp = bob.open_udp(&server);

    // The stranger is registered and knows Bob's ID, but shares no network.
    let payload = Packet::Punch { sender: stranger.peer_id }.to_vec();
    stranger_udp
        .send_to(
            &Packet::RelayForward { dst: bob.peer_id, inner: &payload }.to_vec(),
            server.udp,
        )
        .unwrap();

    let mut buf = [0u8; 1500];
    bob_udp.set_read_timeout(Some(Duration::from_millis(600))).unwrap();
    assert!(
        bob_udp.recv_from(&mut buf).is_err(),
        "the relay must drop traffic between peers with no network in common"
    );
}

/// An unauthenticated party must not be able to claim someone else's peer ID and
/// have their relayed traffic delivered to it.
#[test]
fn the_relay_rejects_registration_without_the_right_token() {
    let server = start_server();
    let mut alice = Client::connect(&server, "alice");
    let mut bob = Client::connect(&server, "bob");

    let network_id = alice.create_network("game night", "pw");
    alice.join(&network_id, "pw").unwrap();
    bob.join(&network_id, "pw").unwrap();
    recv_msg(&mut alice.stream);

    let alice_udp = alice.open_udp(&server);

    // An attacker registers Bob's peer ID from its own socket with a bad token.
    let attacker = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    attacker.set_read_timeout(Some(Duration::from_millis(600))).unwrap();
    attacker
        .send_to(
            &Packet::RelayRegister { peer_id: bob.peer_id, token: [0xFF; 16] }.to_vec(),
            server.udp,
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // Alice sends to Bob; it must not land on the attacker's socket.
    let payload = Packet::Punch { sender: alice.peer_id }.to_vec();
    alice_udp
        .send_to(
            &Packet::RelayForward { dst: bob.peer_id, inner: &payload }.to_vec(),
            server.udp,
        )
        .unwrap();

    let mut buf = [0u8; 1500];
    assert!(
        attacker.recv_from(&mut buf).is_err(),
        "a forged registration must not capture another peer's traffic"
    );
}

/// Deleting removes the network for everyone and tells the other members.
#[test]
fn the_creator_can_delete_a_network_and_members_are_told() {
    let server = start_server();
    let mut alice = Client::connect(&server, "alice");
    let mut bob = Client::connect(&server, "bob");

    let network_id = alice.create_network("game night", "pw");
    alice.join(&network_id, "pw").unwrap();
    bob.join(&network_id, "pw").unwrap();
    recv_msg(&mut alice.stream); // bob arriving

    assert_eq!(alice.delete(&network_id).unwrap(), network_id);

    // Bob is told without having asked.
    match recv_msg(&mut bob.stream) {
        ServerMsg::Deleted { network_id: gone } => assert_eq!(gone, network_id),
        other => panic!("expected deleted, got {other:?}"),
    }

    // And the ID no longer works.
    assert_eq!(
        bob.join(&network_id, "pw").unwrap_err(),
        ErrorCode::NoSuchNetwork
    );
}

/// Knowing the password is not enough to destroy someone else's network.
#[test]
fn a_member_who_did_not_create_it_cannot_delete_it() {
    let server = start_server();
    let mut alice = Client::connect(&server, "alice");
    let mut bob = Client::connect(&server, "bob");

    let network_id = alice.create_network("game night", "pw");
    alice.join(&network_id, "pw").unwrap();
    bob.join(&network_id, "pw").unwrap();
    recv_msg(&mut alice.stream);

    assert_eq!(bob.delete(&network_id).unwrap_err(), ErrorCode::NotOwner);

    // Still there, and still usable.
    let mut carol = Client::connect(&server, "carol");
    assert!(carol.join(&network_id, "pw").is_ok());
}

/// The last person out closes the network.
#[test]
fn a_network_is_removed_once_everyone_has_left() {
    let server = start_server();
    let network_id = {
        let mut alice = Client::connect(&server, "alice");
        let id = alice.create_network("game night", "pw");
        alice.join(&id, "pw").unwrap();
        id
        // alice disconnects here, emptying the network
    };
    std::thread::sleep(Duration::from_millis(400));

    let mut bob = Client::connect(&server, "bob");
    assert_eq!(
        bob.join(&network_id, "pw").unwrap_err(),
        ErrorCode::NoSuchNetwork,
        "the network should be gone once its last member left"
    );
}

/// While somebody is still in it, the network must stay put.
#[test]
fn a_network_survives_while_anyone_remains() {
    let server = start_server();
    let mut alice = Client::connect(&server, "alice");
    let network_id = alice.create_network("game night", "pw");
    alice.join(&network_id, "pw").unwrap();

    {
        let mut bob = Client::connect(&server, "bob");
        bob.join(&network_id, "pw").unwrap();
        recv_msg(&mut alice.stream); // bob arriving
        // bob disconnects here, but alice is still in it
    }
    std::thread::sleep(Duration::from_millis(400));

    let mut carol = Client::connect(&server, "carol");
    assert!(
        carol.join(&network_id, "pw").is_ok(),
        "alice is still a member, so the network must remain"
    );
}

#[test]
fn a_version_mismatch_is_refused() {
    let server = start_server();
    let mut stream = TcpStream::connect(server.control).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    send_msg(
        &mut stream,
        &ClientMsg::Hello {
            version: PROTOCOL_VERSION + 99,
            public_key: [0u8; 32],
            name: "old client".into(),
        },
    );
    match recv_msg(&mut stream) {
        ServerMsg::Error { code, .. } => assert_eq!(code, ErrorCode::VersionMismatch),
        other => panic!("expected a version error, got {other:?}"),
    }
}
