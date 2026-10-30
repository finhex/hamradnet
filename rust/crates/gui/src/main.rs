//! HamRadNet desktop app.
//!
//! One program that is both the window and the tunnel. The daemon runs as
//! background tasks in this same process, so there is nothing to start
//! separately and nothing to configure on the command line - connecting,
//! creating a network and joining one all happen in here.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use hamradnet::config::{default_config_path, Config};
use hamradnet::daemon::{self, Daemon};
use hamradnet::ipc::{Command, PeerReport, Response, Status};

/// How often the window redraws while connected, so peer state looks live.
const REFRESH: Duration = Duration::from_millis(500);

fn main() -> eframe::Result<()> {
    // `sudo` drops DISPLAY and WAYLAND_DISPLAY, so a window cannot be opened and
    // the app dies instantly with an error from deep inside the window library.
    // Say what actually went wrong, and exactly how to fix it.
    #[cfg(unix)]
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("HamRadNet cannot open a window: no display is available.");
        eprintln!();
        eprintln!("This is almost always `sudo` stripping the display settings.");
        eprintln!("`sudo` on its own will not work here. Use one of these instead:");
        eprintln!();
        eprintln!("    ./run-gui.sh                 (does the right thing for you)");
        eprintln!();
        eprintln!("or, by hand:");
        eprintln!();
        eprintln!("    sudo -E env DISPLAY=$DISPLAY \\");
        eprintln!("        WAYLAND_DISPLAY=$WAYLAND_DISPLAY \\");
        eprintln!("        XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR \\");
        eprintln!("        hamradnet-gui");
        eprintln!();
        eprintln!("The app needs root to create the network interface, but it also");
        eprintln!("needs your session's display to draw the window.");
        std::process::exit(1);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hamradnet=info".into()),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 640.0])
            .with_min_inner_size([680.0, 460.0])
            .with_title("HamRadNet"),
        ..Default::default()
    };

    eframe::run_native(
        "HamRadNet",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

/// Result of a command that is still running on the tokio runtime.
#[derive(Default)]
enum Pending {
    #[default]
    Idle,
    Running,
    Done(Response),
}

#[derive(PartialEq)]
enum Dialog {
    None,
    Create,
    Join,
    /// Confirming deletion of the network with this id and name.
    ConfirmDelete(String, String),
}

struct App {
    runtime: tokio::runtime::Runtime,
    config_path: PathBuf,
    daemon: Option<Daemon>,

    // Setup screen.
    server_input: String,
    name_input: String,
    setup_error: Option<String>,

    // Dialogs.
    dialog: Dialog,
    create_name: String,
    create_password: String,
    join_id: String,
    join_password: String,

    pending: Arc<Mutex<Pending>>,
    /// Last message to show the user, and whether it was a failure.
    notice: Option<(String, bool)>,
    elevated: bool,
}

impl App {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("starting the async runtime");

        let config_path = default_config_path();
        let existing = Config::load(&config_path).ok();

        let mut app = App {
            runtime,
            config_path,
            daemon: None,
            server_input: existing
                .as_ref()
                .map(|c| c.server.clone())
                .unwrap_or_default(),
            name_input: existing
                .as_ref()
                .map(|c| c.name.clone())
                .unwrap_or_else(default_name),
            setup_error: None,
            dialog: Dialog::None,
            create_name: String::new(),
            create_password: String::new(),
            join_id: String::new(),
            join_password: String::new(),
            pending: Arc::new(Mutex::new(Pending::Idle)),
            notice: None,
            elevated: is_elevated(),
        };

        // Already set up from a previous run: connect without asking again.
        if existing.is_some() && app.elevated {
            app.connect();
        }
        app
    }

    /// Creates or updates the config, then starts the daemon in this process.
    fn connect(&mut self) {
        let server = self.server_input.trim().to_string();
        if server.is_empty() {
            self.setup_error = Some("Enter your server's address first.".into());
            return;
        }
        // A bare IP is the common case, so fill the port in rather than making
        // people remember it.
        let server = if server.contains(':') {
            server
        } else {
            format!("{server}:{}", hamradnet_proto::DEFAULT_CONTROL_PORT)
        };
        let name = {
            let trimmed = self.name_input.trim();
            if trimmed.is_empty() { default_name() } else { trimmed.to_string() }
        };

        let config = match Config::load(&self.config_path) {
            Ok(mut existing) => {
                existing.server = server;
                existing.name = name;
                if let Err(err) = existing.save(&self.config_path) {
                    self.setup_error = Some(format!("Could not save settings: {err:#}"));
                    return;
                }
                existing
            }
            Err(_) => match Config::generate(server, name) {
                Ok(fresh) => {
                    if let Err(err) = fresh.save(&self.config_path) {
                        self.setup_error = Some(format!("Could not save settings: {err:#}"));
                        return;
                    }
                    fresh
                }
                Err(err) => {
                    self.setup_error = Some(format!("Could not create an identity: {err:#}"));
                    return;
                }
            },
        };

        match self
            .runtime
            .block_on(daemon::spawn(config, self.config_path.clone(), 0))
        {
            Ok(daemon) => {
                self.daemon = Some(daemon);
                self.setup_error = None;
            }
            Err(err) => self.setup_error = Some(format!("Could not start: {err:#}")),
        }
    }

    /// Runs a command on the runtime without blocking the window.
    fn send(&mut self, command: Command) {
        let Some(daemon) = self.daemon.clone() else { return };
        let slot = Arc::clone(&self.pending);
        *slot.lock().unwrap() = Pending::Running;
        self.runtime.spawn(async move {
            let response = hamradnet::ipc_server::dispatch(&daemon, command).await;
            *slot.lock().unwrap() = Pending::Done(response);
        });
    }

    fn is_busy(&self) -> bool {
        matches!(*self.pending.lock().unwrap(), Pending::Running)
    }

    /// Moves a finished command's reply into the notice line.
    fn collect_result(&mut self) {
        let done = {
            let mut guard = self.pending.lock().unwrap();
            match std::mem::take(&mut *guard) {
                Pending::Done(response) => Some(response),
                other => {
                    *guard = other;
                    None
                }
            }
        };
        if let Some(response) = done {
            self.notice = Some(match response {
                Response::Ok { message } => (message, false),
                Response::Error { message } => (message, true),
                _ => ("Done.".into(), false),
            });
            self.dialog = Dialog::None;
        }
    }

    fn snapshot(&self) -> Option<(Status, Vec<PeerReport>)> {
        let daemon = self.daemon.as_ref()?;
        let guard = daemon.inner().lock().unwrap();
        Some((daemon::status(&guard), daemon::peer_reports(&guard)))
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.collect_result();
        ui.ctx().request_repaint_after(REFRESH);

        if self.daemon.is_none() {
            self.setup_screen(ui);
        } else {
            self.main_screen(ui);
        }
    }
}

impl App {
    fn setup_screen(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.heading("HamRadNet");
                ui.label("A private LAN for playing games with friends.");
            });
            ui.add_space(24.0);

            if !self.elevated {
                elevation_warning(ui);
                ui.add_space(12.0);
            }

            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add_space(4.0);

                ui.label("Server address");
                ui.add(
                    egui::TextEdit::singleline(&mut self.server_input)
                        .hint_text("203.0.113.45")
                        .desired_width(f32::INFINITY),
                );
                ui.small("The IP of the server you are hosting. Port 7777 is assumed.");
                ui.add_space(12.0);

                ui.label("Your name");
                ui.add(
                    egui::TextEdit::singleline(&mut self.name_input)
                        .hint_text("what your friends will see")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(14.0);

                ui.horizontal(|ui| {
                    let connect = ui.add_enabled(
                        self.elevated,
                        egui::Button::new("  Connect  ").min_size(egui::vec2(120.0, 30.0)),
                    );
                    if connect.clicked() {
                        self.connect();
                    }
                });
                ui.add_space(4.0);
            });

            if let Some(error) = &self.setup_error {
                ui.add_space(10.0);
                ui.colored_label(egui::Color32::from_rgb(220, 90, 90), error);
            }
        });
    }

    fn main_screen(&mut self, ui: &mut egui::Ui) {
        let Some((status, peers)) = self.snapshot() else { return };
        let ctx = ui.ctx().clone();
        let mut disconnecting = false;

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let (colour, text) = if status.connected {
                    (egui::Color32::from_rgb(70, 190, 120), "Connected")
                } else {
                    (egui::Color32::from_rgb(220, 160, 60), "Connecting…")
                };
                ui.label(egui::RichText::new("●").color(colour).size(16.0));
                ui.label(egui::RichText::new(text).strong());
                ui.label(format!("· {}", status.server));

                if let Some(ip) = status.virtual_ip {
                    ui.separator();
                    ui.label("Your address:");
                    ui.label(egui::RichText::new(ip.to_string()).monospace().strong());
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("Change server")
                        .on_hover_text("Disconnect and pick a different server")
                        .clicked()
                    {
                        disconnecting = true;
                    }
                    ui.label(egui::RichText::new(&status.name).weak());
                });
            });
            ui.add_space(8.0);
        });

        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if self.is_busy() {
                    ui.spinner();
                    ui.label("Working…");
                } else if let Some((message, is_error)) = &self.notice {
                    let colour = if *is_error {
                        egui::Color32::from_rgb(220, 90, 90)
                    } else {
                        egui::Color32::from_rgb(70, 190, 120)
                    };
                    ui.colored_label(colour, message.replace('\n', "  "));
                } else if let Some(error) = &status.last_error {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 160, 60),
                        format!("Cannot reach the server: {error}"),
                    );
                } else {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} connected · {} direct, {} relayed",
                            status.peer_count,
                            status.direct_count,
                            status.peer_count.saturating_sub(status.direct_count)
                        ))
                        .weak(),
                    );
                }
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if !self.elevated {
                elevation_warning(ui);
                ui.add_space(8.0);
            }

            ui.horizontal(|ui| {
                ui.heading("Networks");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Join a network").clicked() {
                        self.dialog = Dialog::Join;
                        self.notice = None;
                    }
                    if ui.button("Create a network").clicked() {
                        self.dialog = Dialog::Create;
                        self.notice = None;
                    }
                });
            });
            ui.add_space(4.0);

            if status.networks.is_empty() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.vertical_centered(|ui| {
                        ui.add_space(10.0);
                        ui.label("You are not in a network yet.");
                        ui.small("Create one and share the ID, or join a friend's.");
                        ui.add_space(10.0);
                    });
                });
            }

            let mut leaving = None;
            let mut deleting = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for network in &status.networks {
                    // Everyone in this particular network, me included, so it
                    // reads as a room rather than one flat list of everybody.
                    let members: Vec<&PeerReport> = peers
                        .iter()
                        .filter(|peer| peer.networks.iter().any(|id| id == &network.network_id))
                        .collect();
                    let total = members.len() + 1;

                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());

                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(&network.name).strong().size(15.0));
                                ui.label(
                                    egui::RichText::new(&network.network_id)
                                        .monospace()
                                        .size(14.0),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .button("Delete")
                                        .on_hover_text(
                                            "Remove for everyone. Only its creator can.",
                                        )
                                        .clicked()
                                    {
                                        deleting = Some((
                                            network.network_id.clone(),
                                            network.name.clone(),
                                        ));
                                    }
                                    if ui
                                        .button("Leave")
                                        .on_hover_text("Leave, but keep the network")
                                        .clicked()
                                    {
                                        leaving = Some(network.network_id.clone());
                                    }
                                    if ui.button("Copy ID").clicked() {
                                        ui.ctx().copy_text(network.network_id.clone());
                                        self.notice =
                                            Some(("Network ID copied.".into(), false));
                                    }
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{total} player{}",
                                            if total == 1 { "" } else { "s" }
                                        ))
                                        .weak(),
                                    );
                                },
                            );
                        });

                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(4.0);

                        egui::Grid::new(format!("members-{}", network.network_id))
                            .num_columns(5)
                            .striped(true)
                            .spacing([16.0, 6.0])
                            .show(ui, |ui| {
                                for heading in
                                    ["Address", "Name", "Status", "Ping", "Traffic"]
                                {
                                    ui.label(egui::RichText::new(heading).weak().size(12.0));
                                }
                                ui.end_row();

                                self.self_row(ui, &status);

                                for peer in &members {
                                    peer_row(ui, peer);
                                }
                            });

                        if members.is_empty() {
                            ui.add_space(6.0);
                            ui.small("Nobody else here yet - send a friend the ID above.");
                        }
                    });
                    ui.add_space(6.0);
                }
            });

            if let Some(network_id) = leaving {
                self.send(Command::LeaveNetwork { network_id });
            }
            if let Some((network_id, name)) = deleting {
                self.dialog = Dialog::ConfirmDelete(network_id, name);
                self.notice = None;
            }
        });

        self.dialogs(&ctx);

        if disconnecting {
            self.disconnect();
        }
    }

    /// Stops the tunnel and returns to the setup screen with the current
    /// settings filled in, so the server can be changed without restarting.
    fn disconnect(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown();
        }
        if let Ok(config) = Config::load(&self.config_path) {
            self.server_input = config.server;
            self.name_input = config.name;
        }
        self.notice = None;
        self.dialog = Dialog::None;
    }

    /// Your own row.
    fn self_row(&self, ui: &mut egui::Ui, status: &Status) {
        ui.label(
            egui::RichText::new(
                status
                    .virtual_ip
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "—".into()),
            )
            .monospace()
            .strong(),
        );
        ui.label(egui::RichText::new(format!("{} (you)", status.name)).strong());

        if status.connected {
            ui.colored_label(egui::Color32::from_rgb(70, 190, 120), "online");
        } else {
            ui.colored_label(egui::Color32::from_rgb(220, 160, 60), "connecting");
        }

        match status.server_rtt_ms {
            Some(ms) => {
                ui.label(format!("{ms:.0} ms")).on_hover_text(
                    "Round trip to the server. Only relayed traffic pays this.",
                );
            }
            None => {
                ui.label(egui::RichText::new("—").weak());
            }
        }

        ui.label(
            egui::RichText::new(traffic_label(status.total_rx, status.total_tx)).weak(),
        );
        ui.end_row();
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let busy = self.is_busy();

        if self.dialog == Dialog::Create {
            let mut open = true;
            egui::Window::new("Create a network")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.set_width(340.0);
                    ui.label("Network name");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.create_name)
                            .hint_text("game night")
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_space(8.0);
                    ui.label("Password");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.create_password)
                            .hint_text("friends will need this")
                            .desired_width(f32::INFINITY),
                    );
                    ui.small("Anyone with the ID and this password can join, so pick a real one.");
                    ui.add_space(12.0);
                    let ready = !self.create_password.trim().is_empty() && !busy;
                    if ui.add_enabled(ready, egui::Button::new("Create")).clicked() {
                        let name = if self.create_name.trim().is_empty() {
                            "my network".to_string()
                        } else {
                            self.create_name.trim().to_string()
                        };
                        self.send(Command::CreateNetwork {
                            name,
                            password: self.create_password.clone(),
                        });
                    }
                    if busy {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Creating…");
                        });
                    }
                });
            if !open {
                self.dialog = Dialog::None;
            }
        }

        if let Dialog::ConfirmDelete(network_id, name) = &self.dialog {
            let (network_id, name) = (network_id.clone(), name.clone());
            let mut open = true;
            egui::Window::new("Delete this network?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.set_width(360.0);
                    ui.label(format!("'{name}' ({network_id})"));
                    ui.add_space(8.0);
                    ui.label(
                        "This removes it for everyone, and the ID stops working. \
                         It cannot be undone.",
                    );
                    ui.small("Only whoever created the network can do this.");
                    ui.add_space(6.0);
                    ui.small(
                        "To stop using it yourself without affecting anyone else, \
                         close this and choose Leave instead.",
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let confirm = ui.add_enabled(
                            !busy,
                            egui::Button::new("Delete it")
                                .fill(egui::Color32::from_rgb(150, 55, 55)),
                        );
                        if confirm.clicked() {
                            self.send(Command::DeleteNetwork {
                                network_id: network_id.clone(),
                            });
                        }
                        if ui.button("Cancel").clicked() {
                            self.dialog = Dialog::None;
                        }
                    });
                    if busy {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Deleting…");
                        });
                    }
                });
            if !open {
                self.dialog = Dialog::None;
            }
        }

        if self.dialog == Dialog::Join {
            let mut open = true;
            egui::Window::new("Join a network")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.set_width(340.0);
                    ui.label("Network ID");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.join_id)
                            .hint_text("HAM-4821-9903")
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_space(8.0);
                    ui.label("Password");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.join_password)
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_space(12.0);
                    let ready = !self.join_id.trim().is_empty() && !busy;
                    if ui.add_enabled(ready, egui::Button::new("Join")).clicked() {
                        self.send(Command::JoinNetwork {
                            network_id: self.join_id.trim().to_uppercase(),
                            password: self.join_password.clone(),
                        });
                    }
                    if busy {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Joining…");
                        });
                    }
                });
            if !open {
                self.dialog = Dialog::None;
            }
        }
    }
}

/// One player's row.
fn peer_row(ui: &mut egui::Ui, peer: &PeerReport) {
    ui.label(egui::RichText::new(peer.virtual_ip.to_string()).monospace());
    ui.label(&peer.name);

    let (colour, label) = match peer.link.as_str() {
        "up" => (egui::Color32::from_rgb(70, 190, 120), "online"),
        "connecting" => (egui::Color32::from_rgb(220, 160, 60), "connecting"),
        _ => (egui::Color32::GRAY, "offline"),
    };
    ui.colored_label(colour, label);

    match peer.rtt_ms {
        Some(ms) => {
            let colour = if ms < 60.0 {
                egui::Color32::from_rgb(70, 190, 120)
            } else if ms < 150.0 {
                egui::Color32::from_rgb(220, 190, 80)
            } else {
                egui::Color32::from_rgb(220, 130, 80)
            };
            // How the traffic is routed is worth knowing but not worth a column
            // of its own, so it lives on the hover instead.
            ui.colored_label(colour, format!("{ms:.0} ms")).on_hover_text(
                match peer.path.as_str() {
                    "direct" => "Connected straight to this player.",
                    "relay" => "Routed through the server, which adds its latency.",
                    _ => "Not connected yet.",
                },
            );
        }
        None => {
            ui.label(egui::RichText::new("—").weak());
        }
    }

    ui.label(egui::RichText::new(traffic_label(peer.rx_bytes, peer.tx_bytes)).weak());
    ui.end_row();
}

fn elevation_warning(ui: &mut egui::Ui) {
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgb(70, 45, 20))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new("This app needs administrator rights")
                    .strong()
                    .color(egui::Color32::from_rgb(250, 200, 120)),
            );
            ui.small(if cfg!(windows) {
                "Close it, right-click hamradnet-gui.exe and choose \"Run as administrator\". \
                 Creating a network adapter is not something Windows allows otherwise."
            } else {
                "Close it and start it again with sudo. Creating a network interface \
                 needs root."
            });
        });
}

fn default_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "player".to_string())
}

#[cfg(unix)]
fn is_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// No clean way to ask without pulling in the Windows API crates, so probe a
/// location only administrators can write to.
#[cfg(windows)]
fn is_elevated() -> bool {
    let probe = std::path::Path::new(r"C:\Windows\hamradnet-elevation-probe.tmp");
    match std::fs::File::create(probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

/// Plain words rather than arrows: the default font has no glyph for the
/// up/down arrows, so they render as empty boxes.
fn traffic_label(rx: u64, tx: u64) -> String {
    format!("{} in · {} out", human_bytes(rx), human_bytes(tx))
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
