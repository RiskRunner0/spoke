/// Async/sync bridge between the Matrix background task and the egui UI.
use std::{path::PathBuf, sync::mpsc};

use matrix_sdk::{
    AuthSession, Client, Room, RoomState,
    config::SyncSettings,
    room::MessagesOptions,
    ruma::{
        OwnedRoomOrAliasId, RoomId, UserId, uint,
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            room::{
                member::{MembershipState, StrippedRoomMemberEvent},
                message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            },
        },
    },
};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::warn;

use spoke_core::{
    matrix::SpokeClient,
    voice::{
        VoiceEvent, VoiceSession,
        events::{VoiceJoinEventContent, VoiceLeaveEventContent, VoiceMuteEventContent},
    },
};

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
    // Voice events
    VoiceJoined { room_id: String },
    VoiceLeft,
    VoiceParticipantsUpdated(Vec<String>),
    // History
    HistoryLoaded { room_id: String, messages: Vec<(String, String)> },
}

#[derive(Debug)]
pub enum AppCommand {
    SendMessage { room_id: String, body: String },
    InviteUser { room_id: String, mxid: String },
    JoinRoom { room_id: String },
    CreateRoom { name: String },
    JoinRoomByAlias { alias: String },
    LeaveRoom { room_id: String },
    // Voice commands
    JoinVoice { room_id: String },
    LeaveVoice,
    MuteVoice { muted: bool },
    // History
    FetchHistory { room_id: String },
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
        let mut voice: Option<VoiceSession> = None;
        let mut voice_room_id: Option<String> = None;
        let sidecar_url = std::env::var("SPOKE_SIDECAR")
            .unwrap_or_else(|_| "http://localhost:8090".into());
        let http = reqwest::Client::new();

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

                // ── Voice commands ─────────────────────────────────────────────

                AppCommand::JoinVoice { room_id } => {
                    // Tear down any existing session first.
                    if let Some(old) = voice.take() {
                        old.disconnect().await;
                    }

                    // Get the Matrix access token.
                    let access_token = match inner.session() {
                        Some(AuthSession::Matrix(s)) => s.tokens.access_token.clone(),
                        _ => {
                            warn!("JoinVoice: no matrix session");
                            send(&tx, &ctx_cmd, AppEvent::Error("not logged in".into()));
                            continue;
                        }
                    };

                    // Send org.spoke.voice.join to the room.
                    let session_id = uuid::Uuid::new_v4().to_string();
                    if let Ok(rid) = RoomId::parse(&room_id) {
                        if let Some(room) = inner.get_room(&rid) {
                            let content = VoiceJoinEventContent { session_id };
                            if let Err(e) = room.send(content).await {
                                warn!("voice join event: {e}");
                            }
                        }
                    }

                    // Ask the sidecar for a LiveKit token.
                    let resp = http
                        .post(format!("{sidecar_url}/_spoke/v1/voice/token"))
                        .bearer_auth(&access_token)
                        .json(&serde_json::json!({"room_id": &room_id}))
                        .send()
                        .await;

                    let resp = match resp {
                        Ok(r) if r.status().is_success() => r,
                        Ok(r) => {
                            warn!("sidecar returned {}", r.status());
                            send(&tx, &ctx_cmd, AppEvent::Error(
                                format!("sidecar error: {}", r.status()),
                            ));
                            continue;
                        }
                        Err(e) => {
                            warn!("sidecar request: {e}");
                            send(&tx, &ctx_cmd, AppEvent::Error(format!("sidecar: {e}")));
                            continue;
                        }
                    };

                    let body: serde_json::Value =
                        match resp.json().await {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("sidecar response parse: {e}");
                                send(&tx, &ctx_cmd, AppEvent::Error(format!("sidecar parse: {e}")));
                                continue;
                            }
                        };

                    let lk_url = body["livekit_url"]
                        .as_str()
                        .unwrap_or("ws://localhost:7880")
                        .to_owned();
                    let lk_token = body["livekit_token"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();

                    // Connect to LiveKit.
                    let (voice_event_tx, mut voice_event_rx) =
                        tokio_mpsc::unbounded_channel::<VoiceEvent>();

                    match VoiceSession::connect(&lk_url, &lk_token, voice_event_tx).await {
                        Ok(session) => {
                            voice = Some(session);
                            voice_room_id = Some(room_id.clone());
                            send(&tx, &ctx_cmd, AppEvent::VoiceJoined { room_id });

                            // Forward VoiceEvents → AppEvents.
                            let tx2 = tx.clone();
                            let ctx2 = ctx_cmd.clone();
                            tokio::spawn(async move {
                                while let Some(ve) = voice_event_rx.recv().await {
                                    match ve {
                                        VoiceEvent::ParticipantsUpdated(ps) => {
                                            send(&tx2, &ctx2, AppEvent::VoiceParticipantsUpdated(ps));
                                        }
                                        VoiceEvent::Error(e) => {
                                            send(&tx2, &ctx2, AppEvent::Error(format!("voice: {e}")));
                                        }
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            warn!("voice connect: {e}");
                            send(&tx, &ctx_cmd, AppEvent::Error(format!("voice: {e}")));
                        }
                    }
                }

                AppCommand::LeaveVoice => {
                    if let Some(session) = voice.take() {
                        session.disconnect().await;
                    }
                    // Send org.spoke.voice.leave.
                    if let Some(rid_str) = voice_room_id.take() {
                        if let Ok(rid) = RoomId::parse(&rid_str) {
                            if let Some(room) = inner.get_room(&rid) {
                                let _ = room.send(VoiceLeaveEventContent {}).await;
                            }
                        }
                    }
                    send(&tx, &ctx_cmd, AppEvent::VoiceLeft);
                }

                AppCommand::MuteVoice { muted } => {
                    if let Some(ref session) = voice {
                        session.set_muted(muted);
                        // Send org.spoke.voice.mute.
                        if let Some(rid_str) = &voice_room_id {
                            if let Ok(rid) = RoomId::parse(rid_str.as_str()) {
                                if let Some(room) = inner.get_room(&rid) {
                                    let _ = room.send(VoiceMuteEventContent { muted }).await;
                                }
                            }
                        }
                    }
                }

                AppCommand::FetchHistory { room_id } => {
                    let Ok(rid) = RoomId::parse(&room_id) else { continue };
                    let Some(room) = inner.get_room(&rid) else { continue };

                    // Fetch up to 50 events; the default (10) is too few.
                    let mut options = MessagesOptions::backward();
                    options.limit = uint!(50);

                    match room.messages(options).await {
                        Ok(response) => {
                            let mut msgs: Vec<(String, String)> = Vec::new();
                            for event in response.chunk {
                                if let Ok(AnySyncTimelineEvent::MessageLike(
                                    AnySyncMessageLikeEvent::RoomMessage(ev),
                                )) = event.raw().deserialize()
                                {
                                    if let Some(original) = ev.as_original() {
                                        if let MessageType::Text(text) =
                                            &original.content.msgtype
                                        {
                                            msgs.push((
                                                original.sender.to_string(),
                                                text.body.clone(),
                                            ));
                                        }
                                    }
                                }
                            }
                            // messages() returns newest-first; reverse to chronological.
                            msgs.reverse();
                            send(
                                &tx,
                                &ctx_cmd,
                                AppEvent::HistoryLoaded { room_id, messages: msgs },
                            );
                        }
                        Err(e) => warn!("fetch history {room_id}: {e}"),
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
