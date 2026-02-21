// spoke-sidecar: validates Matrix access tokens and issues LiveKit JWTs.
// Routes: POST /_spoke/v1/voice/token
//
// Env vars:
//   LIVEKIT_URL     ws://localhost:7880
//   LIVEKIT_KEY     devkey
//   LIVEKIT_SECRET  devsecretatmostthirtytwocharslong
//   MATRIX_SERVER   http://localhost:8448
//   TURN_SECRET     (optional) shared TURN secret
//   TURN_HOST       (optional) TURN hostname
//   PORT            8090 (default)

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    extract::{Json, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::Engine;
use hmac::{Hmac, Mac};
use livekit_api::access_token::{AccessToken, VideoGrants};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use tracing::warn;

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    livekit_url: String,
    livekit_key: String,
    livekit_secret: String,
    turn_secret: Option<String>,
    turn_host: Option<String>,
    matrix_server: String,
    http: reqwest::Client,
}

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenRequest {
    room_id: String,
}

#[derive(Serialize)]
struct TurnServer {
    urls: String,
    username: String,
    credential: String,
}

#[derive(Serialize)]
struct TokenResponse {
    livekit_url: String,
    livekit_token: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    turn_servers: Vec<TurnServer>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = AppState {
        livekit_url: std::env::var("LIVEKIT_URL")
            .unwrap_or_else(|_| "ws://localhost:7880".into()),
        livekit_key: std::env::var("LIVEKIT_KEY")
            .unwrap_or_else(|_| "devkey".into()),
        livekit_secret: std::env::var("LIVEKIT_SECRET")
            .unwrap_or_else(|_| "devsecretatmostthirtytwocharslong".into()),
        turn_secret: std::env::var("TURN_SECRET").ok(),
        turn_host: std::env::var("TURN_HOST").ok(),
        matrix_server: std::env::var("MATRIX_SERVER")
            .unwrap_or_else(|_| "http://localhost:8448".into()),
        http: reqwest::Client::new(),
    };

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8090);

    let app = Router::new()
        .route("/_spoke/v1/voice/token", post(token_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("bind");

    tracing::info!("spoke-sidecar listening on :{port}");
    axum::serve(listener, app).await.expect("serve");
}

// ── Token handler ─────────────────────────────────────────────────────────────

async fn token_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TokenRequest>,
) -> Result<Json<TokenResponse>, StatusCode> {
    // 1. Extract Bearer token from Authorization header.
    let bearer = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_owned();

    // 2. Validate Matrix token via whoami.
    let whoami_resp = state
        .http
        .get(format!(
            "{}/_matrix/client/v3/account/whoami",
            state.matrix_server
        ))
        .bearer_auth(&bearer)
        .send()
        .await
        .map_err(|e| {
            warn!("whoami request failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if !whoami_resp.status().is_success() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let whoami: serde_json::Value = whoami_resp
        .json()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let user_id = whoami["user_id"]
        .as_str()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
        .to_owned();

    // 3. Build a deterministic LiveKit room name from the Matrix room ID.
    let livekit_room =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body.room_id.as_bytes());

    // 4. Generate LiveKit JWT.
    let livekit_token = AccessToken::with_api_key(&state.livekit_key, &state.livekit_secret)
        .with_identity(&user_id)
        .with_name(&user_id)
        .with_grants(VideoGrants {
            room_join: true,
            room: livekit_room,
            can_publish: true,
            can_subscribe: true,
            ..Default::default()
        })
        .to_jwt()
        .map_err(|e| {
            warn!("JWT generation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // 5. Generate TURN credentials (only if TURN_SECRET and TURN_HOST are set).
    let turn_servers = build_turn_servers(&state, &user_id);

    Ok(Json(TokenResponse {
        livekit_url: state.livekit_url.clone(),
        livekit_token,
        turn_servers,
    }))
}

fn build_turn_servers(state: &AppState, user_id: &str) -> Vec<TurnServer> {
    let (Some(secret), Some(host)) = (&state.turn_secret, &state.turn_host) else {
        return vec![];
    };

    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 86400;

    // Standard TURN REST API credential format: username = "timestamp:userid"
    let username = format!("{expiry}:{user_id}");

    let mut mac = match Hmac::<Sha1>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(e) => {
            warn!("HMAC init failed: {e}");
            return vec![];
        }
    };
    mac.update(username.as_bytes());
    let credential =
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    vec![TurnServer {
        urls: format!("turn:{host}:3478"),
        username,
        credential,
    }]
}
