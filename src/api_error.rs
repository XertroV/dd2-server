use tokio::sync::mpsc::error::SendError;

use crate::router::Response;

#[derive(Debug)]
pub enum Error {
    StrErr(String),
    SqlxErr(sqlx::Error),
    SendErr(SendError<Response>),
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::StrErr(s)
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::SqlxErr(e)
    }
}

impl From<SendError<Response>> for Error {
    fn from(e: SendError<Response>) -> Self {
        Error::SendErr(e)
    }
}
