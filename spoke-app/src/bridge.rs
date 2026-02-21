/// Async/sync bridge between the Matrix background task and the egui UI.
///
/// The Matrix sync loop runs on a dedicated tokio runtime in a background
/// thread. It sends `AppEvent`s to the UI via a std::sync::mpsc channel and
/// receives `AppCommand`s via a tokio unbounded channel. The egui Context is
/// passed in so the background task can call `request_repaint()` whenever new
/// data arrives — this wakes the UI without polling.
use std::{path::PathBuf, sync::mpsc};

use matrix_sdk::{
    Room, RoomState,
    ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::warn;

use spoke_core::matrix::SpokeClient;

// ── Event types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RoomInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug)]
pub enum AppEvent {
    Connected { username: String },
    RoomsUpdated(Vec<RoomInfo>),
    Message { room_id: String, sender: String, body: String },
    Error(String),
}

#[derive(Debug)]
pub enum AppCommand {
    SendMessage { room_id: String, body: String },
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Spawn the Matrix task on a background thread with its own tokio runtime.
/// Returns immediately; the task runs for the lifetime of the process.
pub fn spawn_matrix_task(
    event_tx: mpsc::Sender<AppEvent>,
    cmd_rx: tokio_mpsc::UnboundedReceiver<AppCommand>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(matrix_task(event_tx, cmd_rx, ctx));
    });
}

// ── Matrix task ───────────────────────────────────────────────────────────────

async fn matrix_task(
    event_tx: mpsc::Sender<AppEvent>,
    mut cmd_rx: tokio_mpsc::UnboundedReceiver<AppCommand>,
    ctx: egui::Context,
) {
    // Config from env — login UI comes later.
    let homeserver = std::env::var("SPOKE_HS")
        .unwrap_or_else(|_| "http://localhost:8448".into());
    let username = std::env::var("SPOKE_USER").unwrap_or_else(|_| "alice".into());
    let password = std::env::var("SPOKE_PASS").unwrap_or_else(|_| "alicepass".into());
    let db_path = PathBuf::from(format!("/tmp/spoke-app-{username}.db"));

    // Build client.
    let client = match SpokeClient::new(&homeserver, &db_path).await {
        Ok(c) => c,
        Err(e) => {
            send(&event_tx, &ctx, AppEvent::Error(e.to_string()));
            return;
        }
    };

    if let Err(e) = client.register(&username, &password).await {
        warn!("register: {e}");
    }

    if let Err(e) = client.login(&username, &password).await {
        send(&event_tx, &ctx, AppEvent::Error(e.to_string()));
        return;
    }

    send(&event_tx, &ctx, AppEvent::Connected { username: username.clone() });

    // Register message event handler.
    {
        let tx = event_tx.clone();
        let ctx = ctx.clone();
        client.inner.add_event_handler(
            move |event: OriginalSyncRoomMessageEvent, room: Room| {
                let tx = tx.clone();
                let ctx = ctx.clone();
                async move {
                    if room.state() != RoomState::Joined {
                        return;
                    }
                    if let MessageType::Text(text) = event.content.msgtype {
                        send(
                            &tx,
                            &ctx,
                            AppEvent::Message {
                                room_id: room.room_id().to_string(),
                                sender: event.sender.to_string(),
                                body: text.body,
                            },
                        );
                    }
                }
            },
        );
    }

    // Initial sync to hydrate room state.
    if let Err(e) = client.inner.sync_once(Default::default()).await {
        send(&event_tx, &ctx, AppEvent::Error(e.to_string()));
        return;
    }

    send(&event_tx, &ctx, AppEvent::RoomsUpdated(collect_rooms(&client)));

    // Command handler — runs concurrently with the sync loop.
    let inner = client.inner.clone();
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                AppCommand::SendMessage { room_id, body } => {
                    let Ok(rid) = matrix_sdk::ruma::RoomId::parse(&room_id) else {
                        continue;
                    };
                    if let Some(room) = inner.get_room(&rid) {
                        if let Err(e) =
                            room.send(RoomMessageEventContent::text_plain(body)).await
                        {
                            warn!("send error: {e}");
                        }
                    }
                }
            }
        }
    });

    // Sync loop — blocks until the client stops.
    if let Err(e) = client.sync().await {
        warn!("sync ended: {e}");
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn send(tx: &mpsc::Sender<AppEvent>, ctx: &egui::Context, event: AppEvent) {
    let _ = tx.send(event);
    ctx.request_repaint();
}

fn collect_rooms(client: &SpokeClient) -> Vec<RoomInfo> {
    client
        .inner
        .joined_rooms()
        .into_iter()
        .map(|r| RoomInfo {
            name: r.name().unwrap_or_else(|| r.room_id().to_string()),
            id: r.room_id().to_string(),
        })
        .collect()
}
