pub mod client;
pub mod config;
pub mod crop;
pub mod download;
pub mod endpoints;
pub mod error;
pub mod ffmpeg;
pub mod listing;
pub mod model;
pub mod mp4;
pub mod plan;
pub mod ts;
pub mod url_util;

pub use client::{DashcamClient, EndpointProbe};
pub use config::{log_line, Config, DEFAULT_HOST};
pub use crop::{
    clip_duration, crop_clip, locate_and_parse, plan_for_clip, preview_frame, CropReport,
    RemoteClipInfo,
};
pub use error::{Error, Result};
pub use model::{
    human_duration, human_size, parse_timecode, Clip, ClipKind, DeviceInfo, RangeInfo,
};
pub use plan::{ByteRange, Plan};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
