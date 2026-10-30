//! HamRadNet coordinator.
//!
//! Run this on any VPS with a public IP. It does three things:
//!   * keeps the roster of networks and who is in them (TCP),
//!   * tells each client what its public address looks like from outside (UDP),
//!   * relays traffic for peer pairs that cannot reach each other directly (UDP).

mod control;
mod relay;
mod state;

use std::io::IsTerminal;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use hamradnet_proto::{DEFAULT_CONTROL_PORT, DEFAULT_UDP_PORT};
use tokio::net::{TcpListener, UdpSocket};

use state::State;

fn hamradnet_server_grace() -> u64 {
    state::DEFAULT_EMPTY_GRACE
}

#[derive(Parser, Debug)]
#[command(name = "hamradnet-server", version, about = "HamRadNet coordinator and relay")]
struct Args {
    /// Address to bind both listeners to.
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,

    /// TCP port for the control plane. 0 lets the OS choose.
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    control_port: u16,

    /// UDP port for relay and endpoint reflection. 0 lets the OS choose.
    #[arg(long, default_value_t = DEFAULT_UDP_PORT)]
    udp_port: u16,

    /// File networks are saved to, so they survive a restart.
    #[arg(long, default_value = "networks.json")]
    store: PathBuf,

    /// Keep networks in memory only.
    #[arg(long)]
    no_store: bool,

    /// Seconds to keep a network after its last member leaves, before removing
    /// it. 0 removes it the moment everyone has gone. The default gives players
    /// a minute to reconnect after a dropped link without losing the network.
    #[arg(long, default_value_t = hamradnet_server_grace())]
    empty_grace: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Colour only for a terminal: under systemd it would land in the journal as
    // escape codes.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hamradnet_server=info".into()),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let args = Args::parse();
    let store = (!args.no_store).then_some(args.store);
    let state = Arc::new(Mutex::new(State::with_store_and_grace(store, args.empty_grace)));

    let control_addr = SocketAddr::new(args.bind, args.control_port);
    let udp_addr = SocketAddr::new(args.bind, args.udp_port);

    let listener = TcpListener::bind(control_addr)
        .await
        .with_context(|| format!("binding control plane on {control_addr}"))?;
    let socket = UdpSocket::bind(udp_addr)
        .await
        .with_context(|| format!("binding udp plane on {udp_addr}"))?;

    // Report the addresses actually bound, which differ from those requested
    // when a port of 0 asked the OS to choose. Printed on stdout in a fixed
    // format so scripts and tests can read it without parsing the log.
    let (control_addr, udp_addr) = (listener.local_addr()?, socket.local_addr()?);
    println!("listening control={control_addr} udp={udp_addr}");
    use std::io::Write;
    std::io::stdout().flush().ok();

    tracing::info!(%control_addr, %udp_addr, "hamradnet coordinator listening");

    // Remove networks once everybody has left them.
    let sweep_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            ticker.tick().await;
            let removed = sweep_state.lock().unwrap().sweep_empty();
            if removed > 0 {
                tracing::info!(removed, "removed empty networks");
            }
        }
    });

    let control_state = Arc::clone(&state);
    // Must be the port actually bound: with `--udp-port 0` the requested value
    // is 0, and clients would be told to send their relay traffic to port 0.
    let udp_port = udp_addr.port();

    tokio::select! {
        result = control::serve(listener, control_state, udp_port) => result?,
        result = relay::serve(socket, state) => result?,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }

    Ok(())
}
