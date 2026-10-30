//! The daemon half of the local control channel.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use hamradnet_proto::ClientMsg;
use rand::Rng;
use tokio::net::{TcpListener, TcpStream};

use crate::config::ipc_path;
use crate::daemon::{peer_reports, status, Daemon, Event};
use crate::framing::{read_frame, write_frame};
use crate::ipc::{Command, Endpoint, Request, Response};

/// How long a create/join call waits for the coordinator to answer.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn serve(daemon: Daemon) {
    if let Err(err) = run(daemon).await {
        tracing::error!(%err, "local control channel stopped");
    }
}

async fn run(daemon: Daemon) -> Result<()> {
    // Port 0: the OS picks a free port, which we publish in the endpoint file.
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
    let port = listener.local_addr()?.port();
    let token = STANDARD.encode(rand::thread_rng().gen::<[u8; 32]>());

    let path = ipc_path();
    crate::ipc::save_endpoint(&path, &Endpoint { port, token: token.clone() })?;
    #[cfg(unix)]
    if let Some((uid, gid)) = daemon.ipc_owner() {
        use anyhow::Context as _;
        std::os::unix::fs::chown(&path, Some(uid), Some(gid))
            .with_context(|| format!("giving {} to its user", path.display()))?;
    }
    tracing::info!(port, path = %path.display(), "local control channel ready");

    loop {
        let (stream, _) = listener.accept().await?;
        let daemon = daemon.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(stream, daemon, token).await {
                tracing::debug!(%err, "local control request failed");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, daemon: Daemon, token: String) -> Result<()> {
    let Some(request): Option<Request> = read_frame(&mut stream).await? else {
        return Ok(());
    };

    // Anyone who can read the endpoint file may drive the daemon; anyone who
    // cannot, may not.
    if request.token != token {
        write_frame(
            &mut stream,
            &Response::Error { message: "bad token".into() },
        )
        .await?;
        return Ok(());
    }

    let response = dispatch(&daemon, request.command).await;
    write_frame(&mut stream, &response).await?;
    Ok(())
}

/// Executes one command. Shared by the local socket and the in-process GUI.
pub async fn dispatch(daemon: &Daemon, command: Command) -> Response {
    match command {
        Command::Status => Response::Status { status: status(&daemon.inner().lock().unwrap()) },

        Command::Peers => Response::Peers { peers: peer_reports(&daemon.inner().lock().unwrap()) },

        Command::CreateNetwork { name, password } => {
            if !connected(daemon) {
                return not_connected();
            }
            let events = daemon.subscribe();
            daemon.send_to_server(ClientMsg::CreateNetwork {
                name: name.clone(),
                password: password.clone(),
            });
            match await_event(events, |event| match event {
                Event::Created { network_id, name } => Some(Ok((network_id, name))),
                Event::Failed { message } => Some(Err(message)),
                _ => None,
            })
            .await
            {
                Some(Ok((network_id, name))) => {
                    // Creating a network does not put us in it; join so the
                    // password is saved and the tunnel comes up.
                    daemon
                        .inner()
                        .lock()
                        .unwrap()
                        .pending_joins
                        .insert(network_id.clone(), password.clone());
                    daemon.send_to_server(ClientMsg::JoinNetwork {
                        network_id: network_id.clone(),
                        password,
                    });
                    Response::Ok {
                        message: format!(
                            "Created network '{name}'.\n\n  Network ID: {network_id}\n\n\
                             Share that ID and the password with your friends."
                        ),
                    }
                }
                Some(Err(message)) => Response::Error { message },
                None => timed_out(),
            }
        }

        Command::JoinNetwork { network_id, password } => {
            if !connected(daemon) {
                return not_connected();
            }
            let events = daemon.subscribe();
            daemon
                .inner()
                .lock()
                .unwrap()
                .pending_joins
                .insert(network_id.clone(), password.clone());
            daemon.send_to_server(ClientMsg::JoinNetwork {
                network_id: network_id.clone(),
                password,
            });
            match await_event(events, |event| match event {
                Event::Joined { network_id, name } => Some(Ok((network_id, name))),
                Event::Failed { message } => Some(Err(message)),
                _ => None,
            })
            .await
            {
                Some(Ok((id, name))) => {
                    let ip = daemon
                        .inner()
                        .lock()
                        .unwrap()
                        .identity
                        .as_ref()
                        .map(|i| i.virtual_ip.to_string())
                        .unwrap_or_else(|| "unknown".into());
                    Response::Ok {
                        message: format!(
                            "Joined '{name}' ({id}).\nYour address on this network is {ip}."
                        ),
                    }
                }
                Some(Err(message)) => Response::Error { message },
                None => timed_out(),
            }
        }

        Command::LeaveNetwork { network_id } => {
            if !connected(daemon) {
                return not_connected();
            }
            daemon.send_to_server(ClientMsg::LeaveNetwork { network_id: network_id.clone() });
            Response::Ok { message: format!("Left {network_id}.") }
        }

        Command::DeleteNetwork { network_id } => {
            if !connected(daemon) {
                return not_connected();
            }
            let events = daemon.subscribe();
            daemon.send_to_server(ClientMsg::DeleteNetwork { network_id: network_id.clone() });
            match await_event(events, |event| match event {
                Event::Deleted { network_id } => Some(Ok(network_id)),
                Event::Failed { message } => Some(Err(message)),
                _ => None,
            })
            .await
            {
                Some(Ok(id)) => Response::Ok {
                    message: format!("Deleted {id}. Nobody can join it again."),
                },
                Some(Err(message)) => Response::Error { message },
                None => timed_out(),
            }
        }

        Command::Configure { server, name } => match daemon.configure(&server, &name) {
            Ok(()) => {
                let (server, name) = daemon.server_and_name();
                Response::Ok { message: format!("Connecting to {server} as {name}.") }
            }
            Err(err) => Response::Error { message: format!("{err:#}") },
        },

        Command::Logs => match daemon.logs() {
            Some(logs) => Response::Ok { message: logs.snapshot().0 },
            None => Response::Error { message: "this daemon keeps no log of its own".into() },
        },
    }
}

fn connected(daemon: &Daemon) -> bool {
    daemon.inner().lock().unwrap().connected
}

fn not_connected() -> Response {
    Response::Error {
        message: "not connected to the coordinator yet; check `hamradnet status`".into(),
    }
}

fn timed_out() -> Response {
    Response::Error { message: "the coordinator did not answer in time".into() }
}

/// Waits for the first event the predicate accepts, or gives up.
async fn await_event<T>(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    mut predicate: impl FnMut(Event) -> Option<T>,
) -> Option<T> {
    tokio::time::timeout(COMMAND_TIMEOUT, async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let Some(value) = predicate(event) {
                        return Some(value);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}
