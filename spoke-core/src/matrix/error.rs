use thiserror::Error;

#[derive(Debug, Error)]
pub enum MatrixError {
    #[error("matrix sdk error: {0}")]
    Sdk(#[from] matrix_sdk::Error),

    #[error("matrix client build error: {0}")]
    Build(#[from] matrix_sdk::ClientBuildError),

    #[error("invalid user id: {0}")]
    InvalidUserId(String),
}
