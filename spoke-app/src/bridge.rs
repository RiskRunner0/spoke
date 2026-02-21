/// Async/sync bridge between the Matrix background task and the egui UI.
use std::{path::PathBuf, sync::mpsc};

use matrix_sdk::{
    Client, Room, RoomState,
    config::SyncSettings,
    ruma::{
        OwnedRoomOrAliasId, RoomId, UserId,
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::room::{
            member::{MembershipState, StrippedRoomMemberEvent},
            message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
        },
    },
};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::warn;

use spoke_core::matrix::SpokeClient;

// ── Shared types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RoomInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct InviteInfo {
    pub room_id: String,
    pub room_name: String,
    pub inviter: String,
}

#[derive(Debug)]
pub enum AppEvent {
    Connected { username: String },
    RoomsUpdated(Vec<RoomInfo>),
    InvitesUpdated(Vec<InviteInfo>),
    Message { room_id: String, sender: String, body: String },
    Joined { room_id: String },
    Error(String),
}

#[derive(Debug)]
pub enum AppCommand {
    SendMessage { room_id: String, body: String },
    InviteUser { room_id: String, mxid: String },
    JoinRoom { room_id: String },
    CreateRoom { name: String },
    JoinRoomByAlias { alias: String },
    LeaveRoom { room_id: String },
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn spawn_matrix_task(
    event_tx: mpsc::Sender<AppEvent>,
    cmd_rx: tokio_mpsc::UnboundedReceiver<AppCommand>,
    ctx: egui::Context,
    homeserver: String,
    username: String,
    password: String,
) {
    std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(matrix_task(event_tx, cmd_rx, ctx, homeserver, username, password));
    });
}

// ── Matrix task ───────────────────────────────────────────────────────────────

async fn matrix_task(
    event_tx: mpsc::Sender<AppEvent>,
    mut cmd_rx: tokio_mpsc::UnboundedReceiver<AppCommand>,
    ctx: egui::Context,
    homeserver: String,
    username: String,
    password: String,
) {
    let db_path = PathBuf::from(format!("/tmp/spoke-app-{username}.db"));

    let client = match SpokeClient::new(&homeserver, &db_path).await {
        Ok(c) => c,
        Err(e) => { send(&event_tx, &ctx, AppEvent::Error(e.to_string())); return; }
    };

    if let Err(e) = client.register(&username, &password).await {
        warn!("register: {e}");
    }
    if let Err(e) = client.login(&username, &password).await {
        send(&event_tx, &ctx, AppEvent::Error(e.to_string())); return;
    }

    send(&event_tx, &ctx, AppEvent::Connected { username: username.clone() });

    // ── Event handlers ────────────────────────────────────────────────────────

    // Incoming text messages.
    {
        let tx = event_tx.clone();
        let ctx = ctx.clone();
        client.inner.add_event_handler(
            move |event: OriginalSyncRoomMessageEvent, room: Room| {
                let tx = tx.clone(); let ctx = ctx.clone();
                async move {
                    if room.state() != RoomState::Joined { return; }
                    if let MessageType::Text(text) = event.content.msgtype {
                        send(&tx, &ctx, AppEvent::Message {
                            room_id: room.room_id().to_string(),
                            sender: event.sender.to_string(),
                            body: text.body,
                        });
                    }
                }
            },
        );
    }

    // Incoming invites — StrippedRoomMemberEvent fires for invited rooms.
    {
        let tx = event_tx.clone();
        let ctx = ctx.clone();
        client.inner.add_event_handler(
            move |event: StrippedRoomMemberEvent, _room: Room, client: Client| {
                let tx = tx.clone(); let ctx = ctx.clone();
                async move {
                    if event.content.membership != MembershipState::Invite { return; }
                    let Some(user_id) = client.user_id() else { return };
                    if event.state_key != user_id { return; }
                    send(&tx, &ctx, AppEvent::InvitesUpdated(
                        collect_invites_from_client(&client)
                    ));
                }
            },
        );
    }

    // ── Initial sync ──────────────────────────────────────────────────────────

    if let Err(e) = client.inner.sync_once(Default::default()).await {
        send(&event_tx, &ctx, AppEvent::Error(e.to_string())); return;
    }

    send(&event_tx, &ctx, AppEvent::RoomsUpdated(collect_rooms(&client)));
    send(&event_tx, &ctx, AppEvent::InvitesUpdated(collect_invites(&client)));

    // ── Command handler ───────────────────────────────────────────────────────

    let inner = client.inner.clone();
    let tx = event_tx.clone();
    let ctx_cmd = ctx.clone();

    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                AppCommand::SendMessage { room_id, body } => {
                    let Ok(rid) = RoomId::parse(&room_id) else { continue };
                    if let Some(room) = inner.get_room(&rid) {
                        if let Err(e) = room.send(RoomMessageEventContent::text_plain(body)).await {
                            warn!("send: {e}");
                        }
                    }
                }

                AppCommand::InviteUser { room_id, mxid } => {
                    let Ok(rid) = RoomId::parse(&room_id) else { continue };
                    let Ok(uid) = UserId::parse(&mxid) else {
                        warn!("invalid mxid: {mxid}"); continue;
                    };
                    if let Some(room) = inner.get_room(&rid) {
                        if let Err(e) = room.invite_user_by_id(&uid).await {
                            warn!("invite: {e}");
                            send(&tx, &ctx_cmd, AppEvent::Error(e.to_string()));
                        }
                    }
                }

                AppCommand::JoinRoom { room_id } => {
                    let Ok(rid) = RoomId::parse(&room_id) else { continue };
                    match inner.join_room_by_id(&rid).await {
                        Ok(_) => {
                            send(&tx, &ctx_cmd, AppEvent::Joined { room_id });
                            send(&tx, &ctx_cmd, AppEvent::RoomsUpdated(collect_rooms_from_client(&inner)));
                            send(&tx, &ctx_cmd, AppEvent::InvitesUpdated(collect_invites_from_client(&inner)));
                        }
                        Err(e) => {
                            warn!("join: {e}");
                            send(&tx, &ctx_cmd, AppEvent::Error(e.to_string()));
                        }
                    }
                }

                AppCommand::CreateRoom { name } => {
                    let mut req = CreateRoomRequest::new();
                    req.name = Some(name);
                    match inner.create_room(req).await {
                        Ok(resp) => {
                            let room_id = resp.room_id().to_string();
                            send(&tx, &ctx_cmd, AppEvent::Joined { room_id: room_id.clone() });
                            send(&tx, &ctx_cmd, AppEvent::RoomsUpdated(collect_rooms_from_client(&inner)));
                        }
                        Err(e) => {
                            warn!("create_room: {e}");
                            send(&tx, &ctx_cmd, AppEvent::Error(e.to_string()));
                        }
                    }
                }

                AppCommand::JoinRoomByAlias { alias } => {
                    let id: OwnedRoomOrAliasId = match alias.try_into() {
                        Ok(id) => id,
                        Err(e) => { warn!("invalid alias: {e}"); continue; }
                    };
                    match inner.join_room_by_id_or_alias(&id, &[]).await {
                        Ok(room) => {
                            let room_id = room.room_id().to_string();
                            send(&tx, &ctx_cmd, AppEvent::Joined { room_id });
                            send(&tx, &ctx_cmd, AppEvent::RoomsUpdated(collect_rooms_from_client(&inner)));
                        }
                        Err(e) => {
                            warn!("join: {e}");
                            send(&tx, &ctx_cmd, AppEvent::Error(e.to_string()));
                        }
                    }
                }

                AppCommand::LeaveRoom { room_id } => {
                    let Ok(rid) = RoomId::parse(&room_id) else { continue };
                    if let Some(room) = inner.get_room(&rid) {
                        match room.leave().await {
                            Ok(_) => send(&tx, &ctx_cmd, AppEvent::RoomsUpdated(collect_rooms_from_client(&inner))),
                            Err(e) => {
                                warn!("leave: {e}");
                                send(&tx, &ctx_cmd, AppEvent::Error(e.to_string()));
                            }
                        }
                    }
                }
            }
        }
    });

    // Sync loop — manual so we can poll invite/room state after every cycle.
    let mut settings = SyncSettings::default();
    loop {
        match client.inner.sync_once(settings.clone()).await {
            Ok(response) => {
                settings = settings.token(response.next_batch);
                send(&event_tx, &ctx, AppEvent::RoomsUpdated(collect_rooms(&client)));
                send(&event_tx, &ctx, AppEvent::InvitesUpdated(collect_invites(&client)));
            }
            Err(e) => {
                warn!("sync error: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn send(tx: &mpsc::Sender<AppEvent>, ctx: &egui::Context, event: AppEvent) {
    let _ = tx.send(event);
    ctx.request_repaint();
}

fn collect_rooms(client: &SpokeClient) -> Vec<RoomInfo> {
    collect_rooms_from_client(&client.inner)
}

fn collect_rooms_from_client(client: &Client) -> Vec<RoomInfo> {
    client.joined_rooms().into_iter()
        .map(|r| RoomInfo {
            id: r.room_id().to_string(),
            name: r.name().unwrap_or_else(|| r.room_id().to_string()),
        })
        .collect()
}

fn collect_invites(client: &SpokeClient) -> Vec<InviteInfo> {
    collect_invites_from_client(&client.inner)
}

fn collect_invites_from_client(client: &Client) -> Vec<InviteInfo> {
    client.invited_rooms().into_iter()
        .map(|r| InviteInfo {
            room_id: r.room_id().to_string(),
            room_name: r.name().unwrap_or_else(|| r.room_id().to_string()),
            inviter: String::new(),
        })
        .collect()
}
