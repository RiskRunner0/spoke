use eframe::egui;

/// Top-level application state.
///
/// Right now this holds placeholder data so we can build and iterate on the
/// layout. Real Matrix rooms and messages will replace the stubs once the
/// async bridge is wired in.
pub struct SpokeApp {
    /// The room currently selected in the sidebar.
    selected_room: Option<usize>,
    /// Draft message in the input bar.
    input: String,
    // TODO: replace with real room/message state from spoke-core
    rooms: Vec<String>,
    messages: Vec<(String, String)>, // (sender, text)
}

impl SpokeApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            selected_room: Some(0),
            input: String::new(),
            rooms: vec![
                "# general".into(),
                "# voice".into(),
                "# off-topic".into(),
            ],
            messages: vec![
                ("alice".into(), "hello from spoke".into()),
                ("bob".into(), "hey!".into()),
            ],
        }
    }
}

impl eframe::App for SpokeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── Left sidebar: room list ──────────────────────────────────────
        egui::SidePanel::left("rooms")
            .resizable(true)
            .default_width(200.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("Spoke");
                ui.separator();

                for (i, room) in self.rooms.iter().enumerate() {
                    let selected = self.selected_room == Some(i);
                    if ui.selectable_label(selected, room).clicked() {
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
                    let text = std::mem::take(&mut self.input);
                    self.messages.push(("me".into(), text));
                    response.request_focus();
                }
            });
            ui.add_space(6.0);
        });

        // ── Central: message history ─────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            let room_name = self
                .selected_room
                .and_then(|i| self.rooms.get(i))
                .map(|s| s.as_str())
                .unwrap_or("—");

            ui.heading(room_name);
            ui.separator();

            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for (sender, text) in &self.messages {
                        ui.horizontal(|ui| {
                            ui.strong(sender);
                            ui.label(text);
                        });
                    }
                });
        });
    }
}
