use matrix_sdk::{
    Client,
    config::SyncSettings,
    ruma::{
        UserId,
        api::client::{account::register::v3 as register, uiaa::AuthData},
    },
};
use std::path::Path;
use tracing::info;

use crate::matrix::error::MatrixError;

/// Spoke's handle to a Matrix session.
///
/// One instance per logged-in account. Owns the matrix-sdk Client and drives
/// the sync loop. All higher-level operations (rooms, messages, voice events)
/// go through this.
pub struct SpokeClient {
    pub inner: Client,
}

impl SpokeClient {
    /// Build a client pointed at `homeserver_url`, storing session data in
    /// `db_path` (SQLite). Call `login` next.
    pub async fn new(homeserver_url: &str, db_path: &Path) -> Result<Self, MatrixError> {
        let client = Client::builder()
            .homeserver_url(homeserver_url)
            .sqlite_store(db_path, None)
            .build()
            .await?;

        Ok(Self { inner: client })
    }

    /// Password login. Returns early if the session is already restored from
    /// the store (i.e. we've logged in before on this db_path).
    pub async fn login(&self, username: &str, password: &str) -> Result<(), MatrixError> {
        if self.inner.logged_in() {
            info!("session already restored from store, skipping login");
            return Ok(());
        }

        // Accept either a full MXID (@alice:server) or a bare localpart (alice).
        // For bare localparts, derive the server name from the homeserver URL.
        let mxid = if username.starts_with('@') {
            username.to_owned()
        } else {
            let server = self
                .inner
                .homeserver()
                .host_str()
                .unwrap_or("localhost")
                .to_owned();
            format!("@{username}:{server}")
        };

        let user_id = UserId::parse(&mxid)
            .map_err(|e| MatrixError::InvalidUserId(e.to_string()))?;

        self.inner
            .matrix_auth()
            .login_username(user_id, password)
            .initial_device_display_name("Spoke")
            .send()
            .await?;

        info!("logged in as {username}");
        Ok(())
    }

    /// Register a new account on the homeserver.
    ///
    /// Conduit with open registration accepts a simple dummy-auth flow.
    /// Returns Ok(()) if the user already exists — callers can treat
    /// registration and login as a single "ensure account exists" step.
    pub async fn register(&self, username: &str, password: &str) -> Result<(), MatrixError> {
        let mut req = register::Request::new();
        req.username = Some(username.to_owned());
        req.password = Some(password.to_owned());
        req.auth = Some(AuthData::Dummy(Default::default()));

        match self.inner.matrix_auth().register(req).await {
            Ok(_) => {
                info!("registered {username}");
                Ok(())
            }
            Err(matrix_sdk::Error::Http(ref e))
                if e.to_string().contains("M_USER_IN_USE") =>
            {
                info!("{username} already registered");
                Ok(())
            }
            Err(e) => Err(MatrixError::Sdk(e)),
        }
    }

    /// Run the Matrix sync loop. Drives all incoming events.
    ///
    /// This blocks until the client is logged out or the process exits.
    /// Run this on a dedicated tokio task.
    pub async fn sync(&self) -> Result<(), MatrixError> {
        self.inner.sync(SyncSettings::default()).await?;
        Ok(())
    }
}
