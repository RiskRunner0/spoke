//! Minimal end-to-end connectivity test.
//!
//! Registers + logs in a test user, creates a room, sends a message,
//! then runs the sync loop and prints incoming text messages.
//!
//! Prerequisites:
//!   docker compose -f infra/docker-compose.dev.yml up -d
//!
//! Run from the workspace root:
//!   cargo run -p spoke-core --example connect
//!
//! Env vars (all optional, shown with defaults):
//!   SPOKE_HS    http://localhost:8448
//!   SPOKE_USER  alice
//!   SPOKE_PASS  alicepass
//!   RUST_LOG    spoke_core=debug,matrix_sdk=warn

use matrix_sdk::{
    Room, RoomState,
    ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
};
use spoke_core::matrix::SpokeClient;
use std::{env, path::PathBuf};
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            env::var("RUST_LOG")
                .unwrap_or_else(|_| "spoke_core=debug,matrix_sdk=warn".into()),
        )
        .init();

    let homeserver = env::var("SPOKE_HS")
        .unwrap_or_else(|_| "http://localhost:8448".into());
    let username = env::var("SPOKE_USER").unwrap_or_else(|_| "alice".into());
    let password = env::var("SPOKE_PASS").unwrap_or_else(|_| "alicepass".into());

    let db_path = PathBuf::from(format!("/tmp/spoke-dev-{username}.db"));

    info!("connecting to {homeserver} as @{username}:localhost");
    let client = SpokeClient::new(&homeserver, &db_path).await?;

    client.register(&username, &password).await?;
    client.login(&username, &password).await?;

    // Print incoming text messages.
    client.inner.add_event_handler(
        |event: OriginalSyncRoomMessageEvent, room: Room| async move {
            if room.state() != RoomState::Joined {
                return;
            }
            if let MessageType::Text(text) = event.content.msgtype {
                println!(
                    "[{}] {}: {}",
                    room.name().unwrap_or_else(|| room.room_id().as_str().to_owned()),
                    event.sender,
                    text.body,
                );
            }
        },
    );

    // Initial sync to load existing room state.
    info!("initial sync…");
    client.inner.sync_once(Default::default()).await?;

    // Create a room if we have none.
    let rooms = client.inner.joined_rooms();
    if rooms.is_empty() {
        info!("no rooms — creating #spoke-dev");
        let req = matrix_sdk::ruma::api::client::room::create_room::v3::Request::new();
        let resp = client.inner.create_room(req).await?;
        info!("created {}", resp.room_id());
        // Sync once more so the new room shows up in joined_rooms().
        client.inner.sync_once(Default::default()).await?;
    }

    // Send a hello to the first joined room.
    if let Some(room) = client.inner.joined_rooms().into_iter().next() {
        info!("sending hello to {}", room.room_id());
        room.send(RoomMessageEventContent::text_plain("hello from spoke")).await?;
    }

    info!("sync loop running — Ctrl-C to stop");
    client.sync().await?;

    Ok(())
}
