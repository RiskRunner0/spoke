use std::sync::mpsc;

use eframe::egui;
use tokio::sync::mpsc as tokio_mpsc;

use crate::bridge::{AppCommand, AppEvent, RoomInfo};

pub struct SpokeApp {
    event_rx: mpsc::Receiver<AppEvent>,
    cmd_tx: tokio_mpsc::UnboundedSender<AppCommand>,

    status: String,
    rooms: Vec<RoomInfo>,
    selected_room: Option<usize>,
    // All received messages across all rooms: (room_id, sender, body)
    messages: Vec<(String, String, String)>,
    input: String,
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
            selected_room: None,
            messages: Vec::new(),
            input: String::new(),
        }
    }
}

impl eframe::App for SpokeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain all pending events from the Matrix task.
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
                AppEvent::Message { room_id, sender, body } => {
                    self.messages.push((room_id, sender, body));
                }
                AppEvent::Error(e) => {
                    self.status = format!("Error: {e}");
                }
            }
        }

        // ── Left sidebar: room list ───────────────────────────────────────
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
            });

        // ── Bottom input bar ─────────────────────────────────────────────
        egui::TopBottomPanel::bottom("input").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let input_field = egui::TextEdit::singleline(&mut self.input)
                    .hint_text("Message…")
                    .desired_width(ui.available_width() - 60.0);

                let response = ui.add(input_field);
                let send = ui.button("Send");
                let submitted = send.clicked()
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

        // ── Central: message history ──────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            let current = self.selected_room.and_then(|i| self.rooms.get(i));
            let room_name = current.map(|r| r.name.as_str()).unwrap_or("—");
            let room_id = current.map(|r| r.id.as_str());

            ui.heading(room_name);
            ui.separator();

            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for (msg_room_id, sender, body) in &self.messages {
                        if room_id == Some(msg_room_id.as_str()) {
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
