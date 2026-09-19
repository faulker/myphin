use thiserror::Error;

/// Errors the UI can show. Messages must never include Access URLs or tokens.
#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    User(String),
    #[error("internal error")]
    Internal(#[from] InternalError),
}

#[derive(Debug, Error)]
pub enum InternalError {
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("io: {0}")]
    Io(String),
    #[error("crypto")]
    Crypto,
    #[error("http")]
    Http,
    #[error("json")]
    Json,
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        Error::Internal(InternalError::Sqlite(err.to_string()))
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Internal(InternalError::Io(err.to_string()))
    }
}

impl Error {
    pub fn user(msg: impl Into<String>) -> Self {
        Error::User(msg.into())
    }

    /// Public text for the UI. Internal details stay in logs (already redacted).
    pub fn as_user_message(&self) -> String {
        match self {
            Error::User(msg) => msg.clone(),
            Error::Internal(_) => "Something went wrong. Check the log.".to_string(),
        }
    }
}
