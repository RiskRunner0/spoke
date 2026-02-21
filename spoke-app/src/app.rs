use std::sync::mpsc;

use eframe::egui;
use tokio::sync::mpsc as tokio_mpsc;

use crate::bridge::{spawn_matrix_task, AppCommand, AppEvent, InviteInfo, RoomInfo};

pub struct SpokeApp {
    event_rx: mpsc::Receiver<AppEvent>,
    cmd_tx: tokio_mpsc::UnboundedSender<AppCommand>,

    status: String,
    rooms: Vec<RoomInfo>,
    pending_invites: Vec<InviteInfo>,
    selected_room: Option<usize>,
    messages: Vec<(String, String, String)>, // (room_id, sender, body)
    input: String,

    // Invite dialog state.
    show_invite_dialog: bool,
    invite_input: String,

    // Login state.
    logged_in: bool,
    login_homeserver: String,
    login_username: String,
    login_password: String,
    login_error: Option<String>,
    login_connecting: bool,
    pending_spawn: Option<(mpsc::Sender<AppEvent>, tokio_mpsc::UnboundedReceiver<AppCommand>)>,
}

impl SpokeApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        event_rx: mpsc::Receiver<AppEvent>,
        cmd_tx: tokio_mpsc::UnboundedSender<AppCommand>,
        mut pending_spawn: Option<(
            mpsc::Sender<AppEvent>,
            tokio_mpsc::UnboundedReceiver<AppCommand>,
        )>,
    ) -> Self {
        let hs_env = std::env::var("SPOKE_HS").ok();
        let user_env = std::env::var("SPOKE_USER").ok();
        let pass_env = std::env::var("SPOKE_PASS").ok();

        let login_homeserver =
            hs_env.clone().unwrap_or_else(|| "http://localhost:8448".into());
        let login_username = user_env.clone().unwrap_or_default();
        let login_password = pass_env.clone().unwrap_or_default();

        // Auto-submit if all three env vars are set (dev convenience).
        let mut login_connecting = false;
        if hs_env.is_some() && user_env.is_some() && pass_env.is_some() {
            if let Some((event_tx, cmd_rx)) = pending_spawn.take() {
                spawn_matrix_task(
                    event_tx,
                    cmd_rx,
                    cc.egui_ctx.clone(),
                    login_homeserver.clone(),
                    login_username.clone(),
                    login_password.clone(),
                );
                login_connecting = true;
            }
        }

        Self {
            event_rx,
            cmd_tx,
            status: String::new(),
            rooms: Vec::new(),
            pending_invites: Vec::new(),
            selected_room: None,
            messages: Vec::new(),
            input: String::new(),
            show_invite_dialog: false,
            invite_input: String::new(),
            logged_in: false,
            login_homeserver,
            login_username,
            login_password,
            login_error: None,
            login_connecting,
            pending_spawn,
        }
    }
}

impl eframe::App for SpokeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain events from the Matrix task.
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                AppEvent::Connected { username } => {
                    self.logged_in = true;
                    self.login_connecting = false;
                    self.login_password.clear();
                    self.status = format!("@{username}");
                }
                AppEvent::RoomsUpdated(rooms) => {
                    self.rooms = rooms;
                    if self.selected_room.is_none() && !self.rooms.is_empty() {
                        self.selected_room = Some(0);
                    }
                }
                AppEvent::InvitesUpdated(invites) => {
                    self.pending_invites = invites;
                }
                AppEvent::Message { room_id, sender, body } => {
                    self.messages.push((room_id, sender, body));
                }
                AppEvent::Joined { room_id } => {
                    if let Some(i) = self.rooms.iter().position(|r| r.id == room_id) {
                        self.selected_room = Some(i);
                    }
                }
                AppEvent::Error(e) => {
                    if !self.logged_in {
                        // Recreate channels so the user can retry login.
                        let (new_event_tx, new_event_rx) = std::sync::mpsc::channel();
                        let (new_cmd_tx, new_cmd_rx) = tokio::sync::mpsc::unbounded_channel();
                        self.event_rx = new_event_rx;
                        self.cmd_tx = new_cmd_tx;
                        self.pending_spawn = Some((new_event_tx, new_cmd_rx));
                        self.login_connecting = false;
                        self.login_error = Some(e);
                    } else {
                        self.status = format!("Error: {e}");
                    }
                }
            }
        }

        if !self.logged_in {
            self.show_login_panel(ctx);
            return;
        }

        // ── Invite dialog ─────────────────────────────────────────────────────
        if self.show_invite_dialog {
            let mut open = true;
            egui::Window::new("Invite User")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("Matrix ID:");
                    let resp = ui.text_edit_singleline(&mut self.invite_input);

                    if self.invite_input.is_empty() && !resp.has_focus() {
                        ui.small("e.g. @bob:localhost");
                    }

                    ui.horizontal(|ui| {
                        let can_invite = !self.invite_input.is_empty();
                        if ui.add_enabled(can_invite, egui::Button::new("Invite")).clicked() {
                            if let Some(room) =
                                self.selected_room.and_then(|i| self.rooms.get(i))
                            {
                                let _ = self.cmd_tx.send(AppCommand::InviteUser {
                                    room_id: room.id.clone(),
                                    mxid: std::mem::take(&mut self.invite_input),
                                });
                            }
                            self.show_invite_dialog = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.show_invite_dialog = false;
                            self.invite_input.clear();
                        }
                    });
                });
            if !open {
                self.show_invite_dialog = false;
                self.invite_input.clear();
            }
        }

        // ── Left sidebar ──────────────────────────────────────────────────────
        egui::SidePanel::left("rooms")
            .resizable(true)
            .default_width(200.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("Spoke");
                ui.small(&self.status);
                ui.separator();

                for (i, room) in self.rooms.iter().enumerate() {
                    let selected = self.selected_room == Some(i);
                    if ui.selectable_label(selected, &room.name).clicked() {
                        self.selected_room = Some(i);
                    }
                }

                if !self.pending_invites.is_empty() {
                    ui.separator();
                    ui.small("Invites");
                    let invites = self.pending_invites.clone();
                    for invite in invites {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&invite.room_name).italics());
                            if ui.small_button("Join").clicked() {
                                let _ = self.cmd_tx.send(AppCommand::JoinRoom {
                                    room_id: invite.room_id.clone(),
                                });
                            }
                        });
                    }
                }
            });

        // ── Bottom input bar ──────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("input").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let input_field = egui::TextEdit::singleline(&mut self.input)
                    .hint_text("Message…")
                    .desired_width(ui.available_width() - 60.0);

                let response = ui.add(input_field);
                let send_btn = ui.button("Send");
                let submitted = send_btn.clicked()
                    || (response.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter)));

                if submitted && !self.input.is_empty() {
                    if let Some(room) =
                        self.selected_room.and_then(|i| self.rooms.get(i))
                    {
                        let _ = self.cmd_tx.send(AppCommand::SendMessage {
                            room_id: room.id.clone(),
                            body: std::mem::take(&mut self.input),
                        });
                        response.request_focus();
                    }
                }
            });
            ui.add_space(6.0);
        });

        // ── Central: message history ──────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            let current = self.selected_room.and_then(|i| self.rooms.get(i));
            let room_name = current.map(|r| r.name.as_str()).unwrap_or("—");
            let room_id = current.map(|r| r.id.clone());

            ui.horizontal(|ui| {
                ui.heading(room_name);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.selected_room.is_some() && ui.button("Invite…").clicked() {
                        self.show_invite_dialog = true;
                    }
                });
            });
            ui.separator();

            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for (msg_room_id, sender, body) in &self.messages {
                        if room_id.as_deref() == Some(msg_room_id.as_str()) {
                            ui.horizontal(|ui| {
                                ui.strong(sender);
                                ui.label(body);
                            });
                        }
                    }
                });
        });
    }
}

impl SpokeApp {
    fn show_login_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let available_height = ui.available_height();
            ui.add_space(available_height * 0.25);
            ui.vertical_centered(|ui| {
                ui.set_max_width(360.0);
                ui.heading("Spoke");
                ui.add_space(16.0);

                egui::Grid::new("login_fields")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Homeserver");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.login_homeserver)
                                .desired_width(240.0),
                        );
                        ui.end_row();

                        ui.label("Username");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.login_username)
                                .desired_width(240.0),
                        );
                        ui.end_row();

                        ui.label("Password");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.login_password)
                                .password(true)
                                .desired_width(240.0),
                        );
                        ui.end_row();
                    });

                ui.add_space(12.0);

                let can_submit = !self.login_connecting
                    && !self.login_homeserver.is_empty()
                    && !self.login_username.is_empty()
                    && !self.login_password.is_empty();

                let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let login_clicked =
                    ui.add_enabled(can_submit, egui::Button::new("Log in")).clicked();

                if login_clicked || (enter_pressed && can_submit) {
                    if let Some((event_tx, cmd_rx)) = self.pending_spawn.take() {
                        spawn_matrix_task(
                            event_tx,
                            cmd_rx,
                            ctx.clone(),
                            self.login_homeserver.clone(),
                            self.login_username.clone(),
                            self.login_password.clone(),
                        );
                        self.login_connecting = true;
                        self.login_error = None;
                    }
                }

                if self.login_connecting {
                    ui.add_space(8.0);
                    ui.label("Connecting…");
                }

                if let Some(err) = &self.login_error {
                    ui.add_space(8.0);
                    ui.colored_label(egui::Color32::RED, err.as_str());
                }
            });
        });
    }
}
