//! `hamradnet check` - verifies a coordinator is reachable and working.
//!
//! Runs without root and without touching the tunnel interface, so it is safe to
//! run anywhere. It tests both planes, which matters: the control plane is TCP
//! and the traffic that actually carries your games is UDP, and a firewall that
//! allows one while dropping the other is the most common way a setup fails.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hamradnet_proto::{
    wire::Packet, ClientMsg, ServerMsg, MAX_PACKET_SIZE, PROTOCOL_VERSION,
};
use tokio::net::{TcpStream, UdpSocket};

use crate::framing::{read_frame, write_frame};
use crate::session::{public_from_private, NOISE_PARAMS};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UDP_TIMEOUT: Duration = Duration::from_secs(5);
/// Probes sent when measuring. The first is discarded as a warm-up.
const PROBE_COUNT: usize = 8;

pub async fn run(server: &str) -> Result<()> {
    println!("Checking {server}");
    println!();

    // ---------------------------------------------------------- TCP ----
    let started = Instant::now();
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(server))
        .await
        .with_context(|| format!("timed out connecting to {server}"))?
        .with_context(|| format!("could not connect to {server}"))?;
    let tcp_latency = started.elapsed();
    let server_ip = stream.peer_addr()?.ip();
    println!("  [ok]   TCP  connected in {} ms", tcp_latency.as_millis());

    let (mut reader, mut writer) = stream.into_split();

    // A throwaway identity: this connection joins nothing and leaves no trace
    // beyond a peer that disconnects a moment later.
    let keypair = snow::Builder::new(NOISE_PARAMS.parse()?).generate_keypair()?;
    let private: [u8; 32] = keypair
        .private
        .try_into()
        .map_err(|_| anyhow::anyhow!("unexpected key length"))?;

    write_frame(
        &mut writer,
        &ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            public_key: public_from_private(&private)?,
            name: "check".into(),
        },
    )
    .await?;

    let welcome: ServerMsg = tokio::time::timeout(CONNECT_TIMEOUT, read_frame(&mut reader))
        .await
        .context("timed out waiting for a reply")?
        .context("the connection failed while waiting for a reply")?
        .context("the server closed the connection without replying")?;

    let (peer_id, virtual_ip, udp_port) = match welcome {
        ServerMsg::Welcome { version, peer_id, virtual_ip, udp_port, .. } => {
            if version != PROTOCOL_VERSION {
                println!("  [!!]   the server speaks protocol v{version}, this build speaks v{PROTOCOL_VERSION}");
                bail!("version mismatch - update whichever side is older");
            }
            (peer_id, virtual_ip, udp_port)
        }
        ServerMsg::Error { message, .. } => bail!("the server refused us: {message}"),
        other => bail!("unexpected reply from the server: {other:?}"),
    };

    println!("  [ok]   control plane speaking protocol v{PROTOCOL_VERSION}");
    println!("         it would give this machine {virtual_ip} (as peer {peer_id})");

    // ---------------------------------------------------------- UDP ----
    let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
        .await
        .context("binding a local UDP socket")?;
    let server_udp = SocketAddr::new(server_ip, udp_port);

    // Several probes, not one.
    //
    // The first packet on a fresh socket pays for the NAT mapping being created
    // and the route being warmed, which can be several times the steady-state
    // round trip. Reporting that single figure as "the latency" is simply wrong,
    // so the first sample is discarded and the rest are summarised.
    let mut samples = Vec::new();
    let mut buf = vec![0u8; MAX_PACKET_SIZE];
    let mut lost = 0;

    for probe in 0..PROBE_COUNT {
        let txid: u64 = rand::random();
        let sent = Instant::now();
        socket
            .send_to(&Packet::ReflectRequest { txid }.to_vec(), server_udp)
            .await
            .with_context(|| format!("sending UDP to {server_udp}"))?;

        match tokio::time::timeout(UDP_TIMEOUT, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, _))) => match Packet::decode(&buf[..len]) {
                Ok(Packet::ReflectResponse { txid: got, observed }) if got == txid => {
                    // Skip the first: it carries the setup cost.
                    if probe > 0 {
                        samples.push(sent.elapsed());
                    }
                    if probe == 0 {
                        println!("  [ok]   UDP  port {udp_port} answered");
                        println!("         this machine looks like {observed} from outside");
                    }
                }
                _ => lost += 1,
            },
            _ => lost += 1,
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    if samples.is_empty() {
        println!("  [FAIL] UDP  no reply from port {udp_port}");
        println!();
        println!("The control plane works but UDP does not get through, so peers");
        println!("would see each other and still not exchange any traffic.");
        println!("Open UDP {udp_port} on the server and in your provider's firewall");
        println!("or security group, then run this again.");
        bail!("UDP {udp_port} is not reachable");
    }

    samples.sort();
    let best = samples[0];
    let median = samples[samples.len() / 2];
    let worst = samples[samples.len() - 1];
    let jitter = worst.saturating_sub(best);

    println!(
        "  [ok]   UDP  round trip  best {:.0} ms · median {:.0} ms · worst {:.0} ms",
        best.as_secs_f64() * 1000.0,
        median.as_secs_f64() * 1000.0,
        worst.as_secs_f64() * 1000.0,
    );
    if lost > 0 {
        println!("  [!!]   {lost} of {PROBE_COUNT} probes went missing");
    }

    println!();
    println!("Everything works. Point clients at {server}");
    println!();
    // What the number does and does not mean, because it is easy to read too
    // much into it.
    println!("That is the round trip to the server, which is what relayed peers");
    println!("pay. Players who connect directly to each other never touch it, so");
    println!("their latency is the distance between them, not to here.");
    if jitter > Duration::from_millis(60) {
        println!();
        println!("Jitter of {:.0} ms is high; the path to this server is unsteady.", jitter.as_secs_f64() * 1000.0);
    }
    Ok(())
}
