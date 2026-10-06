use serde_json::Value;

use crate::model::{format_device_time, Clip, ClipKind};
use crate::url_util;

const MEDIA_EXTS: [&str; 12] = [
    "mp4", "mov", "ts", "avi", "mkv", "m4v", "3gp", "jpg", "jpeg", "png", "bmp", "thm",
];

pub fn strip_query(path: &str) -> &str {
    path.split(['?', '#']).next().unwrap_or(path)
}

pub fn looks_like_media(path: &str) -> bool {
    let bare = strip_query(path);
    match bare.rsplit_once('.') {
        Some((_, ext)) => MEDIA_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

fn resolve_remote(remote: &str, list_path: &str, file_base: &str) -> String {
    let remote = strip_query(remote).trim();
    if remote.starts_with('/') {
        return remote.to_string();
    }
    // GKU files already include their mount point, e.g. "sd//Video/x.TS".
    if remote.starts_with("sd/") {
        return remote.to_string();
    }
    let dir = if list_path.contains('?') {
        file_base.trim_end_matches('/').to_string()
    } else if list_path.ends_with('/') {
        list_path.trim_end_matches('/').to_string()
    } else {
        url_util::dirname(list_path).to_string()
    };
    format!("{}/{}", dir.trim_end_matches('/'), remote)
}

fn get_str<'a>(map: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    for (k, v) in map {
        if keys.iter().any(|want| k.eq_ignore_ascii_case(want)) {
            if let Some(s) = v.as_str() {
                return Some(s);
            }
        }
    }
    None
}

fn get_u64(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    for (k, v) in map {
        if keys.iter().any(|want| k.eq_ignore_ascii_case(want)) {
            if let Some(n) = v.as_u64() {
                return Some(n);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn get_f64(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f64> {
    for (k, v) in map {
        if keys.iter().any(|want| k.eq_ignore_ascii_case(want)) {
            if let Some(n) = v.as_f64() {
                return Some(n);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.parse::<f64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn entry_to_clip(
    map: &serde_json::Map<String, Value>,
    list_path: &str,
    file_base: &str,
    size_scale: u64,
) -> Option<Clip> {
    let remote = get_str(map, &["name", "path", "file", "filename", "url", "src"])?;
    if !looks_like_media(remote) {
        return None;
    }
    let path = resolve_remote(remote, list_path, file_base);
    let mut clip = Clip::new(&path);
    if let Some(kind) = get_u64(map, &["type", "kind"]).and_then(ClipKind::from_device_type) {
        clip.kind = kind;
    }
    clip.size = get_u64(
        map,
        &["size", "size_bytes", "filesize", "bytes", "file_size"],
    )
    .map(|v| v.saturating_mul(size_scale));
    clip.duration_s = get_f64(
        map,
        &["duration", "duration_s", "length", "seconds", "playtime"],
    )
    .filter(|v| *v > 0.0);
    clip.recorded_at = get_str(
        map,
        &[
            "createtimestr",
            "createtime",
            "time",
            "date",
            "datetime",
            "mtime",
            "created",
            "timestamp",
        ],
    )
    .map(format_device_time);
    Some(clip)
}

fn clips_from_json(
    value: &Value,
    list_path: &str,
    file_base: &str,
    size_scale: u64,
    out: &mut Vec<Clip>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                clips_from_json(item, list_path, file_base, size_scale, out);
            }
        }
        Value::Object(map) => {
            if let Some(clip) = entry_to_clip(map, list_path, file_base, size_scale) {
                out.push(clip);
                return;
            }
            for value in map.values() {
                clips_from_json(value, list_path, file_base, size_scale, out);
            }
        }
        Value::String(s) => {
            if looks_like_media(s) {
                let path = resolve_remote(s, list_path, file_base);
                out.push(Clip::new(&path));
            }
        }
        _ => {}
    }
}

fn clips_from_html(text: &str, list_path: &str, file_base: &str, out: &mut Vec<Clip>) {
    let bytes = text.as_bytes();
    let mut cursor = 0;
    while let Some(pos) = find_ci(&bytes[cursor..], b"href=") {
        let start = cursor + pos + 5;
        if start >= bytes.len() {
            break;
        }
        let quote = bytes[start];
        if quote != b'"' && quote != b'\'' {
            cursor = start;
            continue;
        }
        let content_start = start + 1;
        let Some(end_rel) = bytes[content_start..].iter().position(|b| *b == quote) else {
            break;
        };
        let href = &text[content_start..content_start + end_rel];
        if looks_like_media(href) && !href.contains("..") {
            let path = resolve_remote(href, list_path, file_base);
            out.push(Clip::new(&path));
        }
        cursor = content_start + end_rel;
    }
}

fn clips_from_text(text: &str, list_path: &str, file_base: &str, out: &mut Vec<Clip>) {
    for line in text.lines() {
        let line = line.trim().trim_matches('"').trim_matches('\'');
        if line.is_empty() || line.len() > 512 {
            continue;
        }
        if looks_like_media(line) {
            let path = resolve_remote(line, list_path, file_base);
            out.push(Clip::new(&path));
        }
    }
}

fn find_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

/// Parses a clip listing from a device response. `size_scale` converts listing
/// size units to bytes (GKU firmwares report KiB).
pub fn parse_clip_list(
    body: &[u8],
    list_path: &str,
    file_base: &str,
    size_scale: u64,
) -> Vec<Clip> {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim_start();
    let mut clips = Vec::new();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            clips_from_json(&value, list_path, file_base, size_scale, &mut clips);
        }
    }
    if clips.is_empty() && (text.contains("<a ") || text.contains("<A ") || text.contains("<html"))
    {
        clips_from_html(&text, list_path, file_base, &mut clips);
    }
    if clips.is_empty() {
        clips_from_text(&text, list_path, file_base, &mut clips);
    }
    sort_clips(&mut clips);
    clips.dedup_by(|a, b| a.path == b.path);
    clips
}

pub fn sort_clips(clips: &mut [Clip]) {
    clips.sort_by(|a, b| {
        let ka = a.recorded_at.clone().unwrap_or_else(|| a.name.clone());
        let kb = b.recorded_at.clone().unwrap_or_else(|| b.name.clone());
        kb.cmp(&ka)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const GKU_PAGE: &[u8] = br#"{"result":0,"info":[{"folder":"loop","count":2,"files":[
        {"name":"sd//Video/20261006_174438_F.TS","duration":-1,"type":2,"size":394730,"createtimestr":"20261006174938"},
        {"name":"sd//Photo/20261006_175058_F.JPG","duration":0,"type":1,"size":230,"createtimestr":"20261006175058"}
    ]}]}"#;

    #[test]
    fn parses_gku_listing() {
        let clips = parse_clip_list(
            GKU_PAGE,
            "/app/getfilelist?folder=loop&start=0&end=99",
            "/DCIM/100MEDIA",
            1024,
        );
        assert_eq!(clips.len(), 2);
        let photo = clips.iter().find(|c| c.kind == ClipKind::Photo).unwrap();
        assert_eq!(photo.path, "sd//Photo/20261006_175058_F.JPG");
        assert_eq!(photo.size, Some(230 * 1024));
        assert_eq!(photo.recorded_at.as_deref(), Some("2026-10-06 17:50:58"));
        let video = clips.iter().find(|c| c.kind == ClipKind::Video).unwrap();
        assert_eq!(video.path, "sd//Video/20261006_174438_F.TS");
        assert_eq!(video.size, Some(394730 * 1024));
        assert_eq!(video.duration_s, None, "negative durations are unknown");
        assert!(video.is_mpeg_ts());
    }

    #[test]
    fn parses_json_array() {
        let body = br#"[
            {"name":"2026_1006_170000_0001.MP4","size":52428800,"time":"2026-10-06 17:00:00","duration":60},
            {"name":"2026_1006_170100_0002.MP4","size":52428800,"duration":60}
        ]"#;
        let clips = parse_clip_list(body, "/api/files", "/DCIM/100MEDIA", 1);
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].size, Some(52428800));
        assert!(clips[0].is_video());
    }

    #[test]
    fn parses_nested_json() {
        let body = br#"{"code":0,"data":{"files":[{"path":"/DCIM/100MEDIA/A.MP4"},{"path":"/DCIM/100MEDIA/B.JPG"}]}}"#;
        let clips = parse_clip_list(body, "/?custom=1&cmd=3011&par=0", "/DCIM/100MEDIA", 1);
        assert_eq!(clips.len(), 2);
        assert!(clips.iter().all(|c| c.path.starts_with("DCIM/100MEDIA/")));
    }

    #[test]
    fn parses_html_index() {
        let body = br#"<html><body><a href="A.MP4">A</a> <a href="..">up</a> <a href="B.JPG">B</a></body></html>"#;
        let clips = parse_clip_list(body, "/DCIM/100MEDIA/", "/DCIM/100MEDIA", 1);
        assert_eq!(clips.len(), 2);
        assert!(clips.iter().all(|c| c.path.starts_with("DCIM/100MEDIA/")));
    }

    #[test]
    fn parses_plain_text() {
        let body = b"/DCIM/100MEDIA/A.MP4\nnot a file\n/DCIM/100MEDIA/B.MOV\n";
        let clips = parse_clip_list(body, "/list", "/DCIM/100MEDIA", 1);
        assert_eq!(clips.len(), 2);
    }
}
