use tokio::sync::mpsc::error::SendError;

use crate::{router::Response, ToPlayerMgr};

#[derive(Debug)]
pub enum Error {
    StrErr(String),
    SqlxErr(sqlx::Error),
    SendErr(SendError<Response>),
    SendErrToMgr(SendError<ToPlayerMgr>),
    B64(base64::DecodeError),
    JsonErr(serde_json::Error),
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

impl From<SendError<ToPlayerMgr>> for Error {
    fn from(e: SendError<ToPlayerMgr>) -> Self {
        Error::SendErrToMgr(e)
    }
}

impl From<base64::DecodeError> for Error {
    fn from(e: base64::DecodeError) -> Self {
        Error::B64(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::JsonErr(e)
    }
}
