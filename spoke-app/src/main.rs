// On Windows, don't open a console window behind the app in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bridge;

use app::SpokeApp;
use bridge::spawn_matrix_task;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "spoke=debug,spoke_core=debug,matrix_sdk=warn".into()),
        )
        .init();

    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([800.0, 500.0])
            .with_title("Spoke"),
        ..Default::default()
    };

    eframe::run_native(
        "Spoke",
        options,
        Box::new(|cc| {
            // We have the egui Context here, so we can pass it to the bridge
            // for repaint wakeups.
            spawn_matrix_task(event_tx, cmd_rx, cc.egui_ctx.clone());
            Ok(Box::new(SpokeApp::new(cc, event_rx, cmd_tx)))
        }),
    )
}
