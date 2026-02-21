// Matrix protocol layer — wraps matrix-rust-sdk
// Handles sync, auth, rooms, messages, and E2E encryption.

mod client;
mod error;

pub use client::SpokeClient;
pub use error::MatrixError;
