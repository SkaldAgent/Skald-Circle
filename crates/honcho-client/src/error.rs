use std::error::Error as StdError;
use std::fmt;

#[derive(Debug)]
pub enum HonchoError {
    Http { status: u16, body: String },
    Request(reqwest::Error),
    Json(serde_json::Error),
}

impl fmt::Display for HonchoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http { status, body } => write!(f, "HTTP error {status}: {body}"),
            Self::Request(e) => {
                // reqwest's own Display stops at "error sending request for url
                // (...)" and hides the transport cause. Walk the source chain so
                // the real reason (e.g. "Connection reset by peer", "operation
                // timed out") shows up in logs instead of a useless top line.
                write!(f, "Request failed: {e}")?;
                let mut src = e.source();
                while let Some(cause) = src {
                    write!(f, ": {cause}")?;
                    src = cause.source();
                }
                Ok(())
            }
            Self::Json(e) => write!(f, "JSON error: {e}"),
        }
    }
}

impl std::error::Error for HonchoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Request(e) => Some(e),
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for HonchoError {
    fn from(e: reqwest::Error) -> Self {
        Self::Request(e)
    }
}

impl From<serde_json::Error> for HonchoError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}


pub type Result<T> = std::result::Result<T, HonchoError>;
