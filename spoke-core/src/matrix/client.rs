use std::path::{Path, PathBuf};

use matrix_sdk::{
    AuthSession, Client,
    config::SyncSettings,
    matrix_auth::MatrixSession,
    ruma::{
        UserId,
        api::client::{account::register::v3 as register, uiaa::AuthData},
    },
};
use tracing::{info, warn};

use crate::matrix::error::MatrixError;

/// Spoke's handle to a Matrix session.
pub struct SpokeClient {
    pub inner: Client,
    db_path: PathBuf,
}

impl SpokeClient {
    /// Build a client pointed at `homeserver_url`, storing session data in
    /// `db_path` (a directory — matrix-sdk creates SQLite files inside it).
    ///
    /// If the crypto store directory exists but no session file is present
    /// the state is inconsistent (e.g. after a code update that added session
    /// persistence). The stale store is wiped so the next login is clean.
    pub async fn new(homeserver_url: &str, db_path: &Path) -> Result<Self, MatrixError> {
        let session_path = Self::session_path_for(db_path);

        if db_path.exists() && !session_path.exists() {
            warn!("crypto store present but no session file — wiping stale store");
            let _ = std::fs::remove_dir_all(db_path);
        }

        let client = Client::builder()
            .homeserver_url(homeserver_url)
            .sqlite_store(db_path, None)
            .build()
            .await?;

        Ok(Self { inner: client, db_path: db_path.to_owned() })
    }

    /// Restore a previous session or perform a fresh password login.
    ///
    /// On first run: logs in, saves the session to `{db_path}.session.json`.
    /// On subsequent runs: restores from the session file (no network round
    /// trip, same device ID, crypto store stays consistent).
    pub async fn login(&self, username: &str, password: &str) -> Result<(), MatrixError> {
        if self.inner.logged_in() {
            info!("already logged in, skipping");
            return Ok(());
        }

        let session_path = Self::session_path_for(&self.db_path);

        // Try to restore a saved session first.
        if let Some(session) = Self::load_session(&session_path) {
            match self.inner.restore_session(session).await {
                Ok(()) => {
                    info!("session restored from {session_path:?}");
                    return Ok(());
                }
                Err(e) => {
                    // Stale session (token expired, server wiped, etc).
                    // Delete it and fall through to fresh login.
                    warn!("session restore failed ({e}), doing fresh login");
                    let _ = std::fs::remove_file(&session_path);
                }
            }
        }

        // Fresh password login.
        let mxid = self.full_mxid(username);
        let user_id = UserId::parse(&mxid)
            .map_err(|e| MatrixError::InvalidUserId(e.to_string()))?;

        self.inner
            .matrix_auth()
            .login_username(user_id, password)
            .initial_device_display_name("Spoke")
            .send()
            .await?;

        info!("logged in as {mxid}");

        // Persist the session so the next startup can restore it.
        if let Some(AuthSession::Matrix(session)) = self.inner.session() {
            match serde_json::to_string(&session) {
                Ok(json) => { let _ = std::fs::write(&session_path, json); }
                Err(e) => warn!("failed to serialise session: {e}"),
            }
        }

        Ok(())
    }

    /// Register a new account. Returns Ok(()) if the user already exists.
    pub async fn register(&self, username: &str, password: &str) -> Result<(), MatrixError> {
        let mut req = register::Request::new();
        req.username = Some(username.to_owned());
        req.password = Some(password.to_owned());
        req.auth = Some(AuthData::Dummy(Default::default()));

        match self.inner.matrix_auth().register(req).await {
            Ok(_) => { info!("registered {username}"); Ok(()) }
            Err(matrix_sdk::Error::Http(ref e))
                if e.to_string().contains("M_USER_IN_USE") =>
            {
                info!("{username} already registered");
                Ok(())
            }
            Err(e) => Err(MatrixError::Sdk(e)),
        }
    }

    /// Run the Matrix sync loop. Blocks until the client stops.
    /// Run on a dedicated tokio task.
    pub async fn sync(&self) -> Result<(), MatrixError> {
        self.inner.sync(SyncSettings::default()).await?;
        Ok(())
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn session_path_for(db_path: &Path) -> PathBuf {
        db_path.with_extension("session.json")
    }

    fn load_session(path: &Path) -> Option<MatrixSession> {
        let json = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&json).ok()
    }

    /// Build a full MXID from a bare username using the homeserver's host.
    fn full_mxid(&self, username: &str) -> String {
        if username.starts_with('@') {
            return username.to_owned();
        }
        let server = self.inner.homeserver()
            .host_str()
            .unwrap_or("localhost")
            .to_owned();
        format!("@{username}:{server}")
    }
}
