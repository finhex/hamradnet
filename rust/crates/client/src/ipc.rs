//! Local control channel between the `hamradnet` CLI and the running daemon.
//!
//! Loopback TCP rather than a unix socket or a named pipe, so one implementation
//! covers both platforms. The daemon writes the chosen port and a random token
//! to an owner-only file; a caller that cannot read that file cannot drive the
//! daemon.

use std::net::Ipv4Addr;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::framing::{read_frame, write_frame};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub token: String,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Status,
    Peers,
    CreateNetwork { name: String, password: String },
    JoinNetwork { network_id: String, password: String },
    LeaveNetwork { network_id: String },
    DeleteNetwork { network_id: String },
}

/// Note the struct variants. This enum is internally tagged, and serde cannot
/// serialise a tagged newtype variant that wraps a sequence - `Peers(Vec<_>)`
/// fails at runtime, not at compile time. Keeping every variant a struct keeps
/// that trap shut.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Response {
    Status { status: Status },
    Peers { peers: Vec<PeerReport> },
    Ok { message: String },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub connected: bool,
    pub server: String,
    pub name: String,
    pub peer_id: Option<u32>,
    pub virtual_ip: Option<Ipv4Addr>,
    pub public_endpoint: Option<String>,
    pub networks: Vec<NetworkReport>,
    pub peer_count: usize,
    pub direct_count: usize,
    /// Round trip to the coordinator in milliseconds.
    #[serde(default)]
    pub server_rtt_ms: Option<f32>,
    #[serde(default)]
    pub total_rx: u64,
    #[serde(default)]
    pub total_tx: u64,
    /// Why the connection is down, when it is.
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkReport {
    pub network_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerReport {
    pub peer_id: u32,
    pub name: String,
    pub virtual_ip: Ipv4Addr,
    /// "up", "connecting", or "idle".
    pub link: String,
    /// "direct", "relay", or "-".
    pub path: String,
    pub endpoint: Option<String>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Seconds since the last packet from this peer.
    pub last_seen_secs: Option<u64>,
    /// Smoothed round-trip time in milliseconds, once measured.
    #[serde(default)]
    pub rtt_ms: Option<f32>,
    /// Which networks this player shares with you.
    #[serde(default)]
    pub networks: Vec<String>,
}

pub fn load_endpoint(path: &Path) -> Result<Endpoint> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "cannot read {}. Is the daemon running? Start it with `hamradnet up`.",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes).context("malformed ipc endpoint file")
}

pub fn save_endpoint(path: &Path, endpoint: &Endpoint) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(endpoint)?)?;
    restrict(path)
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

/// Sends one command to the daemon and returns its reply.
pub async fn send(path: &Path, command: Command) -> Result<Response> {
    let endpoint = load_endpoint(path)?;
    let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port))
        .await
        .with_context(|| {
            format!("connecting to the daemon on 127.0.0.1:{}", endpoint.port)
        })?;
    let request = Request { token: endpoint.token, command };
    write_frame(&mut stream, &request).await?;
    match read_frame::<_, Response>(&mut stream).await? {
        Some(response) => Ok(response),
        None => bail!("daemon closed the connection without replying"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant must survive a round trip. `Peers` in particular: as a
    /// newtype variant wrapping a `Vec` it serialised fine in tests of the inner
    /// type but failed only when actually sent, taking the connection down.
    #[test]
    fn every_response_variant_round_trips() {
        let status = Status {
            connected: true,
            server: "vpn.example.com:7777".into(),
            name: "alice".into(),
            peer_id: Some(1),
            virtual_ip: Some(Ipv4Addr::new(10, 66, 0, 2)),
            public_endpoint: Some("203.0.113.9:41234".into()),
            networks: vec![NetworkReport {
                network_id: "HAM-1111-2222".into(),
                name: "game night".into(),
            }],
            peer_count: 1,
            direct_count: 1,
            server_rtt_ms: Some(31.0),
            total_rx: 100,
            total_tx: 200,
            last_error: None,
        };
        let peer = PeerReport {
            peer_id: 2,
            name: "bob".into(),
            virtual_ip: Ipv4Addr::new(10, 66, 0, 3),
            link: "up".into(),
            path: "direct".into(),
            endpoint: Some("203.0.113.8:7000".into()),
            rx_bytes: 100,
            tx_bytes: 200,
            last_seen_secs: Some(1),
            rtt_ms: Some(23.5),
            networks: vec!["HAM-1111-2222".into()],
        };

        for response in [
            Response::Status { status },
            Response::Peers { peers: vec![peer] },
            Response::Ok { message: "done".into() },
            Response::Error { message: "nope".into() },
        ] {
            let encoded = serde_json::to_string(&response)
                .unwrap_or_else(|err| panic!("{response:?} failed to serialise: {err}"));
            serde_json::from_str::<Response>(&encoded)
                .unwrap_or_else(|err| panic!("{response:?} failed to deserialise: {err}"));
        }
    }

    #[test]
    fn every_command_round_trips() {
        for command in [
            Command::Status,
            Command::Peers,
            Command::CreateNetwork { name: "n".into(), password: "p".into() },
            Command::JoinNetwork { network_id: "HAM-1-2".into(), password: "p".into() },
            Command::LeaveNetwork { network_id: "HAM-1-2".into() },
            Command::DeleteNetwork { network_id: "HAM-1-2".into() },
        ] {
            let request = Request { token: "t".into(), command };
            let encoded = serde_json::to_string(&request).unwrap();
            serde_json::from_str::<Request>(&encoded).unwrap();
        }
    }
}
