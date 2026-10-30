//! Control plane listener: one task per connected client.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use hamradnet_proto::{
    control::MAX_FRAME_SIZE, ClientMsg, ErrorCode, PeerId, ServerMsg, PROTOCOL_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{
    tcp::{OwnedReadHalf, OwnedWriteHalf},
    TcpListener, TcpStream,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use crate::state::State;

/// A client that says nothing at all for this long is considered gone. Clients
/// ping every 30s, so this tolerates two missed pings.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

pub async fn serve(listener: TcpListener, state: Arc<Mutex<State>>, udp_port: u16) -> Result<()> {
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(%err, "accept failed");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, addr, Arc::clone(&state), udp_port).await {
                tracing::debug!(%addr, %err, "connection closed");
            }
        });
    }
}

async fn read_frame(reader: &mut OwnedReadHalf) -> Result<Option<ClientMsg>> {
    let mut len_bytes = [0u8; 4];
    match reader.read_exact(&mut len_bytes).await {
        Ok(_) => {}
        // A clean EOF is an ordinary disconnect, not an error worth logging.
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let len = u32::from_be_bytes(len_bytes);
    if len > MAX_FRAME_SIZE {
        bail!("control frame of {len} bytes exceeds the limit");
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf).context("malformed control frame")?))
}

async fn write_frame(writer: &mut OwnedWriteHalf, msg: &ServerMsg) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    writer.write_all(&(body.len() as u32).to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    udp_port: u16,
) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (mut reader, mut writer) = stream.into_split();

    // The first frame must be Hello, and it must arrive promptly.
    let hello = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut reader))
        .await
        .context("timed out waiting for hello")??;
    let Some(ClientMsg::Hello { version, public_key, name }) = hello else {
        bail!("expected hello as the first frame");
    };

    if version != PROTOCOL_VERSION {
        let msg = ServerMsg::Error {
            code: ErrorCode::VersionMismatch,
            message: format!("server speaks protocol v{PROTOCOL_VERSION}, client sent v{version}"),
        };
        write_frame(&mut writer, &msg).await.ok();
        bail!("version mismatch: {version}");
    }

    let name = sanitise_name(&name);
    let (tx, rx) = unbounded_channel();
    let Some((peer_id, relay_token)) = state
        .lock()
        .unwrap()
        .register_peer(name.clone(), public_key, tx)
    else {
        let msg = ServerMsg::Error {
            code: ErrorCode::Internal,
            message: "no virtual addresses left".into(),
        };
        write_frame(&mut writer, &msg).await.ok();
        bail!("address pool exhausted");
    };

    tracing::info!(peer_id, %name, %addr, "peer connected");

    let virtual_ip = state
        .lock()
        .unwrap()
        .peers
        .get(&peer_id)
        .map(|peer| peer.virtual_ip)
        .context("peer vanished during registration")?;

    let welcome = ServerMsg::Welcome {
        version: PROTOCOL_VERSION,
        peer_id,
        virtual_ip,
        prefix_len: hamradnet_proto::VIRTUAL_PREFIX_LEN,
        udp_port,
        relay_token,
    };
    write_frame(&mut writer, &welcome).await?;

    // One task owns the write half and drains the notification channel, so any
    // task holding the state lock can push a message without awaiting.
    let writer_task = tokio::spawn(pump_notifications(writer, rx));

    let result = read_loop(&mut reader, peer_id, Arc::clone(&state)).await;

    state.lock().unwrap().remove_peer(peer_id);
    writer_task.abort();
    result
}

async fn pump_notifications(mut writer: OwnedWriteHalf, mut rx: UnboundedReceiver<ServerMsg>) {
    while let Some(msg) = rx.recv().await {
        if write_frame(&mut writer, &msg).await.is_err() {
            break;
        }
    }
}

async fn read_loop(
    reader: &mut OwnedReadHalf,
    peer_id: PeerId,
    state: Arc<Mutex<State>>,
) -> Result<()> {
    loop {
        let frame = tokio::time::timeout(IDLE_TIMEOUT, read_frame(reader))
            .await
            .context("peer idle for too long")??;
        let Some(msg) = frame else { return Ok(()) };

        match msg {
            ClientMsg::Hello { .. } => bail!("duplicate hello"),

            ClientMsg::CreateNetwork { name, password } => {
                if password.is_empty() {
                    send(&state, peer_id, error(ErrorCode::BadPassword, "password required"));
                    continue;
                }
                let name = sanitise_name(&name);
                let network_id = {
                    let mut guard = state.lock().unwrap();
                    // The creator's key, recorded so only they can delete it.
                    let owner = guard
                        .peers
                        .get(&peer_id)
                        .map(|peer| peer.public_key)
                        .unwrap_or([0u8; 32]);
                    guard.create_network(name.clone(), &password, owner)
                };
                tracing::info!(peer_id, %network_id, %name, "network created");
                send(&state, peer_id, ServerMsg::NetworkCreated { network_id, name });
            }

            ClientMsg::JoinNetwork { network_id, password } => {
                let outcome = state.lock().unwrap().join_network(peer_id, &network_id, &password);
                let reply = match outcome {
                    Ok((network, peers)) => ServerMsg::Joined { network, peers },
                    Err(code) => error(code, describe(code)),
                };
                send(&state, peer_id, reply);
            }

            ClientMsg::LeaveNetwork { network_id } => {
                state.lock().unwrap().leave_network(peer_id, &network_id);
                send(&state, peer_id, ServerMsg::Left { network_id });
            }

            ClientMsg::DeleteNetwork { network_id } => {
                let outcome = state.lock().unwrap().delete_network(peer_id, &network_id);
                if let Err(code) = outcome {
                    send(&state, peer_id, error(code, describe(code)));
                }
                // On success every member, this peer included, is told by the
                // state layer, so there is nothing to send here.
            }

            ClientMsg::UpdateEndpoints { endpoints } => {
                let endpoints = endpoints.into_iter().take(8).collect();
                state.lock().unwrap().update_endpoints(peer_id, Some(endpoints), None);
            }

            ClientMsg::Ping => send(&state, peer_id, ServerMsg::Pong),
        }
    }
}

fn send(state: &Arc<Mutex<State>>, peer_id: PeerId, msg: ServerMsg) {
    if let Some(peer) = state.lock().unwrap().peers.get(&peer_id) {
        let _ = peer.tx.send(msg);
    }
}

fn error(code: ErrorCode, message: impl Into<String>) -> ServerMsg {
    ServerMsg::Error { code, message: message.into() }
}

fn describe(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::NoSuchNetwork => "no network with that ID",
        ErrorCode::BadPassword => "wrong password",
        ErrorCode::AlreadyJoined => "already a member of that network",
        ErrorCode::NetworkFull => "that network is full",
        ErrorCode::NotOwner => "only whoever created that network can delete it",
        ErrorCode::VersionMismatch => "protocol version mismatch",
        ErrorCode::RateLimited => "slow down",
        ErrorCode::Internal => "internal error",
    }
}

/// Names are shown in other people's peer lists, so strip control characters and
/// cap the length rather than trusting whatever the client sent.
fn sanitise_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(32)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "anonymous".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned_up() {
        assert_eq!(sanitise_name("konako"), "konako");
        assert_eq!(sanitise_name("  spaced  "), "spaced");
        assert_eq!(sanitise_name(""), "anonymous");
        assert_eq!(sanitise_name("   "), "anonymous");
        assert_eq!(sanitise_name("bad\nname\r\0"), "badname");
        assert_eq!(sanitise_name(&"x".repeat(100)).len(), 32);
    }
}
