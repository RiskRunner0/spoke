use std::sync::mpsc;

use eframe::egui;
use tokio::sync::mpsc as tokio_mpsc;

use crate::bridge::{AppCommand, AppEvent, InviteInfo, RoomInfo};

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
}

impl SpokeApp {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        event_rx: mpsc::Receiver<AppEvent>,
        cmd_tx: tokio_mpsc::UnboundedSender<AppCommand>,
    ) -> Self {
        Self {
            event_rx,
            cmd_tx,
            status: "Connecting…".into(),
            rooms: Vec::new(),
            pending_invites: Vec::new(),
            selected_room: None,
            messages: Vec::new(),
            input: String::new(),
            show_invite_dialog: false,
            invite_input: String::new(),
        }
    }
}

impl eframe::App for SpokeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain events from the Matrix task.
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                AppEvent::Connected { username } => {
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
                    // Select the room we just joined.
                    if let Some(i) = self.rooms.iter().position(|r| r.id == room_id) {
                        self.selected_room = Some(i);
                    }
                }
                AppEvent::Error(e) => {
                    self.status = format!("Error: {e}");
                }
            }
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

                    // Hint text via label when empty.
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

                // Joined rooms.
                for (i, room) in self.rooms.iter().enumerate() {
                    let selected = self.selected_room == Some(i);
                    if ui.selectable_label(selected, &room.name).clicked() {
                        self.selected_room = Some(i);
                    }
                }

                // Pending invites.
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
                    if self.selected_room.is_some()
                        && ui.button("Invite…").clicked()
                    {
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
