//! `hamradnet` - the HamRadNet client.
//!
//! `hamradnet up` runs the daemon, which owns the tunnel. Every other
//! subcommand is a thin client that talks to that daemon over a loopback socket.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use hamradnet::config::{default_config_path, encode_key, ipc_path, normalize_server, Config};
use hamradnet::daemon;
use hamradnet::ipc::{self, Command, Response};
use hamradnet::logs::LogBuffer;

#[derive(Parser, Debug)]
#[command(
    name = "hamradnet",
    version,
    about = "Virtual LAN for playing games with friends over the internet"
)]
struct Cli {
    /// Path to the configuration file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the daemon. Needs root on Linux, Administrator on Windows.
    Up {
        /// Coordinator address, `host:port`. Saved on first run.
        #[arg(long)]
        server: Option<String>,
        /// Name your friends will see. Saved on first run.
        #[arg(long)]
        name: Option<String>,
        /// Local UDP port. 0 lets the OS choose.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Let this user's app and command line control the daemon, for
        /// running it as a service.
        #[arg(long)]
        ipc_user: Option<String>,
    },
    /// Point the running daemon at a server; the command-line twin of the
    /// app's Connect button.
    Connect {
        /// The server's address. Port 7777 is assumed.
        #[arg(long)]
        server: String,
        /// The name your friends see. Unchanged if not given.
        #[arg(long)]
        name: Option<String>,
    },
    /// Show the running daemon's recent log.
    Logs,
    /// Create a new network and join it.
    Create {
        /// A name for the network, for display only.
        name: String,
        /// Password friends will need in order to join.
        #[arg(long)]
        password: String,
    },
    /// Join an existing network.
    Join {
        /// The network ID, e.g. HAM-4821-9903.
        network_id: String,
        #[arg(long)]
        password: String,
    },
    /// Leave a network and stop rejoining it.
    Leave { network_id: String },
    /// Delete a network for everyone. Only its creator can.
    Delete { network_id: String },
    /// Show the connection state.
    Status,
    /// List everyone in your networks.
    Peers,
    /// Test that a coordinator is reachable. Needs no root and changes nothing.
    Check {
        /// Coordinator address. Defaults to the one in the config file.
        #[arg(long)]
        server: Option<String>,
    },
    /// Print this machine's public key.
    Key,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);

    match cli.command {
        Commands::Up { server, name, port, ipc_user } => {
            run_daemon(config_path, server, name, port, ipc_user).await
        }
        Commands::Connect { server, name } => {
            call(Command::Configure { server, name: name.unwrap_or_default() }).await
        }
        Commands::Logs => call(Command::Logs).await,
        Commands::Create { name, password } => {
            call(Command::CreateNetwork { name, password }).await
        }
        Commands::Join { network_id, password } => {
            call(Command::JoinNetwork { network_id, password }).await
        }
        Commands::Leave { network_id } => call(Command::LeaveNetwork { network_id }).await,
        Commands::Delete { network_id } => call(Command::DeleteNetwork { network_id }).await,
        Commands::Status => call(Command::Status).await,
        Commands::Peers => call(Command::Peers).await,
        Commands::Check { server } => {
            let server = match server {
                Some(server) => server,
                None => Config::load(&config_path)
                    .map(|config| config.server)
                    .context("no --server given and no config file to take one from")?,
            };
            hamradnet::check::run(&server).await
        }
        Commands::Key => {
            let config = Config::load(&config_path)?;
            println!("{}", encode_key(&config.public_key()?));
            Ok(())
        }
    }
}

async fn run_daemon(
    config_path: PathBuf,
    server: Option<String>,
    name: Option<String>,
    port: u16,
    ipc_user: Option<String>,
) -> Result<()> {
    // The log goes to stderr and also into a buffer the app can fetch, since a
    // service's output otherwise only reaches the system journal. Plain text,
    // as escape codes would land in both.
    let logs = LogBuffer::default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hamradnet=info".into()),
        )
        .with_writer(logs.clone())
        .with_ansi(false)
        .init();
    let ipc_owner = ipc_user.as_deref().map(lookup_user).transpose()?;
    let server = server.map(|server| normalize_server(&server)).filter(|s| !s.is_empty());

    let config = match Config::load(&config_path) {
        Ok(mut existing) => {
            // Flags override what is on disk, and the change sticks.
            let mut changed = false;
            if let Some(server) = server {
                if existing.server != server {
                    existing.server = server;
                    changed = true;
                }
            }
            if let Some(name) = name {
                if existing.name != name {
                    existing.name = name;
                    changed = true;
                }
            }
            if changed {
                existing.save(&config_path)?;
            }
            existing
        }
        Err(_) => {
            // First run: an identity key is generated and kept from here on. With
            // no server yet, as when started as a service, the daemon waits for
            // one.
            // A service runs as root, so the user it serves names it.
            let name = name.or(ipc_user).unwrap_or_else(default_name);
            let config = Config::generate(server.unwrap_or_default(), name)?;
            config.save(&config_path)?;
            tracing::info!(path = %config_path.display(), "created a new configuration");
            config
        }
    };

    tracing::info!(
        server = %config.server,
        name = %config.name,
        key = %encode_key(&config.public_key()?),
        "starting hamradnet"
    );

    daemon::run(config, config_path, port, daemon::Options { logs: Some(logs), ipc_owner }).await
}

/// The `(uid, gid)` of a user, for `--ipc-user`.
#[cfg(unix)]
fn lookup_user(name: &str) -> Result<(u32, u32)> {
    let c_name = std::ffi::CString::new(name)?;
    // getpwnam returns static storage, read straight away and not kept.
    let entry = unsafe { libc::getpwnam(c_name.as_ptr()) };
    if entry.is_null() {
        anyhow::bail!("--ipc-user: there is no user named {name}");
    }
    Ok(unsafe { ((*entry).pw_uid, (*entry).pw_gid) })
}

#[cfg(not(unix))]
fn lookup_user(_name: &str) -> Result<(u32, u32)> {
    anyhow::bail!("--ipc-user is only used on Linux")
}

fn default_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "player".to_string())
}

/// Sends one command to the daemon and prints the reply for a human.
async fn call(command: Command) -> Result<()> {
    let response = ipc::send(&ipc_path(), command).await?;
    match response {
        Response::Ok { message } => println!("{message}"),
        Response::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        Response::Status { status } => print_status(&status),
        Response::Peers { peers } => print_peers(&peers),
    }
    Ok(())
}

fn print_status(status: &ipc::Status) {
    println!(
        "coordinator : {} ({})",
        status.server,
        if status.connected { "connected" } else { "disconnected" }
    );
    println!("name        : {}", status.name);
    if let Some(ip) = status.virtual_ip {
        println!("address     : {ip}");
    }
    if let Some(endpoint) = &status.public_endpoint {
        println!("public addr : {endpoint}");
    }
    println!(
        "peers       : {} ({} direct, {} relayed)",
        status.peer_count,
        status.direct_count,
        status.peer_count.saturating_sub(status.direct_count)
    );
    if status.networks.is_empty() {
        println!("networks    : none - use `hamradnet join <id> --password <pw>`");
    } else {
        println!("networks    :");
        for network in &status.networks {
            println!("  {}  {}", network.network_id, network.name);
        }
    }
}

fn print_peers(peers: &[ipc::PeerReport]) {
    if peers.is_empty() {
        println!("No peers yet. Nobody else has joined your networks.");
        return;
    }
    println!(
        "{:<16} {:<18} {:<12} {:<8} {:>8} {:>10} {:>10}",
        "ADDRESS", "NAME", "LINK", "PATH", "PING", "RX", "TX"
    );
    for peer in peers {
        println!(
            "{:<16} {:<18} {:<12} {:<8} {:>8} {:>10} {:>10}",
            peer.virtual_ip.to_string(),
            truncate(&peer.name, 18),
            peer.link,
            peer.path,
            match peer.rtt_ms {
                Some(ms) => format!("{ms:.0} ms"),
                None => "-".to_string(),
            },
            human_bytes(peer.rx_bytes),
            human_bytes(peer.tx_bytes),
        );
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        text.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
