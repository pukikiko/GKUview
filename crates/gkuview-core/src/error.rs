use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("network error: {0}")]
    Network(String),

    #[error("HTTP {status} from {url}")]
    HttpStatus { status: u16, url: String },

    #[error("range request not honoured: expected 206, got {0}")]
    RangeUnsupported(u16),

    #[error(
        "the dashcam does not support HTTP range requests; partial clip download is impossible"
    )]
    NoRangeSupport,

    #[error("device returned unexpected data: {0}")]
    Protocol(String),

    #[error("could not locate MP4 'moov' atom in {0}")]
    MoovNotFound(String),

    #[error("MP4 parse error: {0}")]
    Mp4(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("ffmpeg error: {0}")]
    Ffmpeg(String),

    #[error("ffmpeg was not found on PATH; install ffmpeg to preview or export clips")]
    FfmpegMissing,

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("{0}")]
    Other(String),
}

impl From<ureq::Error> for Error {
    fn from(value: ureq::Error) -> Self {
        match value {
            ureq::Error::StatusCode(code) => Error::HttpStatus {
                status: code,
                url: String::new(),
            },
            other => Error::Network(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
