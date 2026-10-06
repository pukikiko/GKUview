use serde::{Deserialize, Serialize};

use crate::url_util;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    Video,
    Photo,
    Other,
}

impl ClipKind {
    pub fn from_path(path: &str) -> Self {
        let ext = path
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "mp4" | "mov" | "ts" | "avi" | "mkv" | "m4v" | "3gp" => ClipKind::Video,
            "jpg" | "jpeg" | "png" | "bmp" => ClipKind::Photo,
            _ => ClipKind::Other,
        }
    }

    /// GKU/Shenshu listings label photos as type 1 and videos as type 2.
    pub fn from_device_type(value: u64) -> Option<Self> {
        match value {
            1 => Some(ClipKind::Photo),
            2 => Some(ClipKind::Video),
            _ => None,
        }
    }
}

/// Formats a GKU `createtimestr` (`YYYYMMDDHHMMSS`) for display.
pub fn format_device_time(value: &str) -> String {
    if value.len() == 14 && value.bytes().all(|b| b.is_ascii_digit()) {
        format!(
            "{}-{}-{} {}:{}:{}",
            &value[0..4],
            &value[4..6],
            &value[6..8],
            &value[8..10],
            &value[10..12],
            &value[12..14]
        )
    } else {
        value.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    pub name: String,
    pub path: String,
    pub size: Option<u64>,
    pub recorded_at: Option<String>,
    pub duration_s: Option<f64>,
    pub kind: ClipKind,
}

impl Clip {
    pub fn new(path: &str) -> Self {
        let path = path.trim_start_matches('/').to_string();
        Self {
            name: url_util::basename(&path).to_string(),
            kind: ClipKind::from_path(&path),
            path,
            size: None,
            recorded_at: None,
            duration_s: None,
        }
    }

    /// True when the clip is an MPEG-TS recording (GKU D600Pro and relatives).
    pub fn is_mpeg_ts(&self) -> bool {
        self.name.to_ascii_lowercase().ends_with(".ts")
    }

    pub fn is_video(&self) -> bool {
        self.kind == ClipKind::Video
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub host: String,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub raw: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct RangeInfo {
    pub status: u16,
    pub content_length: Option<u64>,
    pub accept_ranges: bool,
    pub content_range: Option<(u64, u64, Option<u64>)>,
}

impl RangeInfo {
    pub fn supports_ranges(&self) -> bool {
        self.accept_ranges || self.status == 206 || self.content_range.is_some()
    }

    pub fn total_size(&self) -> Option<u64> {
        if let Some((_, _, Some(total))) = self.content_range {
            return Some(total);
        }
        self.content_length
    }
}

#[derive(Debug, Clone, Default)]
pub struct ListEndpointReport {
    pub label: String,
    pub path: String,
    pub status: u16,
    pub clips: usize,
    pub ok: bool,
    pub note: String,
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn human_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--".to_string();
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

pub fn parse_timecode(input: &str) -> Option<f64> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let mut total = 0.0f64;
    let mut parts = 0;
    for part in input.split(':') {
        let value: f64 = part.trim().parse().ok()?;
        total = total * 60.0 + value;
        parts += 1;
    }
    if parts == 0 {
        None
    } else {
        Some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecode_parsing() {
        assert_eq!(parse_timecode("90"), Some(90.0));
        assert_eq!(parse_timecode("01:30"), Some(90.0));
        assert_eq!(parse_timecode("00:01:30.5"), Some(90.5));
        assert_eq!(parse_timecode("nope"), None);
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(65.0), "01:05");
        assert_eq!(human_duration(3661.0), "1:01:01");
    }

    #[test]
    fn classification() {
        assert_eq!(ClipKind::from_path("A.MP4"), ClipKind::Video);
        assert_eq!(ClipKind::from_path("A.jpg"), ClipKind::Photo);
        assert_eq!(ClipKind::from_path("A.txt"), ClipKind::Other);
    }
}
