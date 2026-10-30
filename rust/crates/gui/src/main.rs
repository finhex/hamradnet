//! HamRadNet desktop app.
//!
//! One program that is both the window and the tunnel. The daemon runs as
//! background tasks in this same process, so there is nothing to start
//! separately and nothing to configure on the command line - connecting,
//! creating a network and joining one all happen in here.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use hamradnet::config::{default_config_path, ipc_path, normalize_server, Config};
use hamradnet::daemon::{self, Daemon};
use hamradnet::ipc::{self, Command, PeerReport, Response, Status};
use hamradnet::logs::LogBuffer;

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
        eprintln!("    sudo -E env -u WAYLAND_DISPLAY -u XDG_RUNTIME_DIR \\");
        eprintln!("        DISPLAY=$DISPLAY XAUTHORITY=$XAUTHORITY \\");
        eprintln!("        hamradnet-gui");
        eprintln!();
        eprintln!("The app needs root to create the network interface, but it also");
        eprintln!("needs your session's display to draw the window.");
        std::process::exit(1);
    }

    // Everything logged goes to the terminal as before, and is also kept for the
    // app's own log window. Plain text, so a copied log pastes cleanly.
    let logs = LogBuffer::default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hamradnet=info".into()),
        )
        .with_writer(logs.clone())
        .with_ansi(false)
        .init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = std::env::consts::OS, "starting HamRadNet");

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
        Box::new(|cc| Ok(Box::new(App::new(cc, logs)))),
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

/// What the window drives. Either the tunnel runs inside this app, which then
/// needs root or Administrator, or it runs as the HamRadNet service and the
/// app, running as an ordinary user, talks to it over the local control
/// channel, the same one the command line uses.
enum Backend {
    Embedded(Daemon),
    Service(Arc<ServiceView>),
}

/// The service as last seen. Asking it is a request over the local control
/// channel, so a background task keeps this current and the window reads it,
/// rather than every redraw waiting on a request.
#[derive(Default)]
struct ServiceView {
    /// The last answer that came back.
    snapshot: Mutex<Option<(Status, Vec<PeerReport>)>>,
    /// Why the last request failed, if it did.
    error: Mutex<Option<String>>,
    log: Mutex<String>,
    /// Fetch the log too, while the log window is open.
    want_log: AtomicBool,
}

async fn fetch_snapshot() -> anyhow::Result<(Status, Vec<PeerReport>)> {
    let path = ipc_path();
    let status = match ipc::send(&path, Command::Status).await? {
        Response::Status { status } => status,
        Response::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!("unexpected reply to a status request"),
    };
    let peers = match ipc::send(&path, Command::Peers).await? {
        Response::Peers { peers } => peers,
        _ => Vec::new(),
    };
    Ok((status, peers))
}

struct App {
    runtime: tokio::runtime::Runtime,
    config_path: PathBuf,
    backend: Option<Backend>,
    /// Showing the setup screen rather than the networks.
    setup: bool,

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

    // Log window.
    logs: LogBuffer,
    show_logs: bool,
    log_notice: Option<String>,
}

impl App {
    fn new(_cc: &eframe::CreationContext<'_>, logs: LogBuffer) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("starting the async runtime");

        let config_path = default_config_path();
        let existing = Config::load(&config_path).ok();

        let mut app = App {
            runtime,
            config_path,
            backend: None,
            setup: true,
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
            logs,
            show_logs: false,
            log_notice: None,
        };

        if let Some(view) = app.find_service() {
            // The tunnel already runs as a service: drive that, with no root
            // needed.
            tracing::info!("found the HamRadNet service; this app controls it");
            if let Some((status, _)) = view.snapshot.lock().unwrap().as_ref() {
                app.server_input = status.server.clone();
                if !status.name.is_empty() {
                    app.name_input = status.name.clone();
                }
                app.setup = status.server.is_empty();
            }
            app.backend = Some(Backend::Service(view));
        } else if existing.is_some() && app.elevated {
            // Already set up from a previous run: connect without asking again.
            app.connect();
        }
        app
    }

    /// The running service, if there is one this user may drive, with a task
    /// keeping its view current. The install script sets it up so that the user
    /// who installed it can.
    fn find_service(&self) -> Option<Arc<ServiceView>> {
        let first = self.runtime.block_on(fetch_snapshot()).ok()?;
        let view = Arc::new(ServiceView::default());
        *view.snapshot.lock().unwrap() = Some(first);
        let polled = Arc::clone(&view);
        self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(REFRESH).await;
                match fetch_snapshot().await {
                    Ok(snapshot) => {
                        *polled.snapshot.lock().unwrap() = Some(snapshot);
                        *polled.error.lock().unwrap() = None;
                    }
                    Err(err) => *polled.error.lock().unwrap() = Some(format!("{err:#}")),
                }
                if polled.want_log.load(Ordering::Relaxed) {
                    if let Ok(Response::Ok { message }) = ipc::send(&ipc_path(), Command::Logs).await {
                        *polled.log.lock().unwrap() = message;
                    }
                }
            }
        });
        Some(view)
    }

    fn is_service(&self) -> bool {
        matches!(self.backend, Some(Backend::Service(_)))
    }

    /// Points the service at the chosen server, or, with no service, creates or
    /// updates the config and starts the daemon in this process.
    fn connect(&mut self) {
        // A bare IP is the common case, so the port is filled in rather than
        // making people remember it.
        let server = normalize_server(&self.server_input);
        if server.is_empty() {
            self.setup_error = Some("Enter your server's address first.".into());
            return;
        }
        let name = {
            let trimmed = self.name_input.trim();
            if trimmed.is_empty() { default_name() } else { trimmed.to_string() }
        };

        if self.is_service() {
            let reply = self.runtime.block_on(ipc::send(&ipc_path(), Command::Configure { server, name }));
            match reply {
                Ok(Response::Ok { .. }) => {
                    self.setup_error = None;
                    self.setup = false;
                }
                Ok(Response::Error { message }) => self.setup_error = Some(message),
                Ok(_) => self.setup_error = Some("Unexpected reply from the service.".into()),
                Err(err) => {
                    self.setup_error = Some(format!("The HamRadNet service did not answer: {err:#}"))
                }
            }
            return;
        }

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

        let options = daemon::Options { logs: Some(self.logs.clone()), ipc_owner: None };
        match self
            .runtime
            .block_on(daemon::spawn(config, self.config_path.clone(), 0, options))
        {
            Ok(daemon) => {
                self.backend = Some(Backend::Embedded(daemon));
                self.setup = false;
                self.setup_error = None;
            }
            Err(err) => self.setup_error = Some(format!("Could not start: {err:#}")),
        }
    }

    /// Runs a command on the runtime without blocking the window.
    fn send(&mut self, command: Command) {
        let slot = Arc::clone(&self.pending);
        match &self.backend {
            None => return,
            Some(Backend::Embedded(daemon)) => {
                let daemon = daemon.clone();
                *slot.lock().unwrap() = Pending::Running;
                self.runtime.spawn(async move {
                    let response = hamradnet::ipc_server::dispatch(&daemon, command).await;
                    *slot.lock().unwrap() = Pending::Done(response);
                });
            }
            Some(Backend::Service(_)) => {
                *slot.lock().unwrap() = Pending::Running;
                self.runtime.spawn(async move {
                    let response = ipc::send(&ipc_path(), command).await.unwrap_or_else(|err| {
                        Response::Error {
                            message: format!("The HamRadNet service did not answer: {err:#}"),
                        }
                    });
                    *slot.lock().unwrap() = Pending::Done(response);
                });
            }
        }
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
        match self.backend.as_ref()? {
            Backend::Embedded(daemon) => {
                let guard = daemon.inner().lock().unwrap();
                Some((daemon::status(&guard), daemon::peer_reports(&guard)))
            }
            Backend::Service(view) => view.snapshot.lock().unwrap().clone(),
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.collect_result();
        ui.ctx().request_repaint_after(REFRESH);

        if let Some(Backend::Service(view)) = &self.backend {
            if let Some(err) = view.error.lock().unwrap().clone() {
                self.notice = Some((format!("The HamRadNet service is not answering: {err}"), true));
            }
        }
        if self.setup || self.backend.is_none() {
            self.setup_screen(ui);
        } else {
            self.main_screen(ui);
        }
        let ctx = ui.ctx().clone();
        self.log_window(&ctx);
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

            if self.is_service() {
                ui.label("HamRadNet runs as a service on this computer. Choose a server to connect it to.");
                ui.add_space(12.0);
            } else if !self.elevated {
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
                        self.elevated || self.is_service(),
                        egui::Button::new("  Connect  ").min_size(egui::vec2(120.0, 30.0)),
                    );
                    if connect.clicked() {
                        self.connect();
                    }
                    if ui.button("Log").on_hover_text("What the app has been doing").clicked() {
                        self.show_logs = true;
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
                // Drawn rather than written: the bundled fonts have no "●".
                let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 5.0, colour);
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
                    if ui.button("Log").on_hover_text("What the app has been doing").clicked() {
                        self.show_logs = true;
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

    /// Returns to the setup screen with the current settings filled in, so the
    /// server can be changed without restarting. A tunnel in this process is
    /// stopped; the service keeps running until it is given the new server.
    fn disconnect(&mut self) {
        match &self.backend {
            Some(Backend::Service(view)) => {
                if let Some((status, _)) = view.snapshot.lock().unwrap().as_ref() {
                    self.server_input = status.server.clone();
                    self.name_input = status.name.clone();
                }
            }
            _ => {
                if let Some(Backend::Embedded(daemon)) = self.backend.take() {
                    daemon.shutdown();
                }
                if let Ok(config) = Config::load(&self.config_path) {
                    self.server_input = config.server;
                    self.name_input = config.name;
                }
            }
        }
        self.setup = true;
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

    /// The log in a window of its own. It follows new lines as they come, and
    /// Copy all puts the whole log on the clipboard, ready to paste into a chat
    /// or an issue.
    fn log_window(&mut self, ctx: &egui::Context) {
        // The log of whatever runs the tunnel: the service's own while the app
        // drives it, otherwise this app's, which includes a tunnel run
        // in-process.
        let service = match &self.backend {
            Some(Backend::Service(view)) => Some(Arc::clone(view)),
            _ => None,
        };
        if let Some(view) = &service {
            view.want_log.store(self.show_logs, Ordering::Relaxed);
        }
        if !self.show_logs {
            return;
        }
        let text = match &service {
            Some(view) => view.log.lock().unwrap().clone(),
            None => self.logs.snapshot().0,
        };
        let count = if text.is_empty() { 0 } else { text.lines().count() };
        let lines = if count == 1 { "1 line".to_string() } else { format!("{count} lines") };
        let mut open = true;
        egui::Window::new("Log")
            .open(&mut open)
            .default_size([860.0, 440.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Copy all").clicked() {
                        ctx.copy_text(text.clone());
                        self.log_notice = Some(format!("Copied {lines} to the clipboard"));
                    }
                    ui.label(self.log_notice.clone().unwrap_or_else(|| lines.clone()));
                });
                ui.separator();
                let body = if text.is_empty() { "Nothing logged yet." } else { text.as_str() };
                egui::ScrollArea::both()
                    .auto_shrink(false)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(body).monospace())
                                .selectable(true)
                                .extend(),
                        );
                    });
            });
        if !open {
            self.show_logs = false;
            self.log_notice = None;
        }
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
                "Close it and start it again with the run-gui.sh launcher, which asks for \
                 root, or install the HamRadNet service once (install.sh in the service \
                 download), after which the app needs no password. Creating a network \
                 interface needs root."
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
