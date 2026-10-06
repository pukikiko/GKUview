use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::client::DashcamClient;
use crate::download::{download_full_to, download_ranges_to, DownloadStats};
use crate::error::{Error, Result};
use crate::ffmpeg;
use crate::model::Clip;
use crate::mp4::{scan_boxes_at, BoxHeader, Mp4Index};
use crate::plan::{self, ByteRange, Plan};
use crate::ts;

const WINDOW: u64 = 1024 * 1024;
const MAX_SCAN_WINDOWS: usize = 64;

#[derive(Debug, Clone)]
pub struct RemoteClipInfo {
    pub size: u64,
    pub accepts_ranges: bool,
    pub index: Mp4Index,
    pub moov_at_end: bool,
}

#[derive(Debug, Clone)]
pub struct CropReport {
    pub output: PathBuf,
    pub clip_name: String,
    pub source_size: u64,
    pub downloaded_bytes: u64,
    pub requests: u64,
    pub planned_bytes: u64,
    pub start_s: f64,
    pub end_s: f64,
    pub keyframe_start_s: f64,
    pub keyframe_aligned: bool,
    pub output_duration_s: Option<f64>,
    pub used_range_requests: bool,
    pub fallback_reason: Option<String>,
}

impl CropReport {
    pub fn saved_percent(&self) -> f64 {
        if self.source_size == 0 {
            0.0
        } else {
            (1.0 - self.downloaded_bytes as f64 / self.source_size as f64) * 100.0
        }
    }
}

fn find_box(boxes: &[BoxHeader], kind: &[u8; 4]) -> Option<BoxHeader> {
    boxes.iter().copied().find(|b| &b.kind == kind)
}

fn fetch_window(client: &DashcamClient, url: &str, start: u64, end: u64) -> Result<Vec<u8>> {
    let (body, _) = client.get_range(url, start, end)?;
    Ok(body)
}

fn fetch_box(client: &DashcamClient, url: &str, header: &BoxHeader) -> Result<(Vec<u8>, u64)> {
    let start = header.start;
    let end = header.end().saturating_sub(1);
    let (body, _) = client.get_range(url, start, end)?;
    if body.len() as u64 != header.size {
        return Err(Error::Protocol(format!(
            "short read for box {}: expected {} bytes, got {}",
            String::from_utf8_lossy(&header.kind),
            header.size,
            body.len()
        )));
    }
    Ok((body, start))
}

pub fn locate_and_parse(client: &DashcamClient, url: &str) -> Result<RemoteClipInfo> {
    let head = client.head(url)?;
    let size = head.total_size().ok_or_else(|| {
        Error::Protocol("device did not report a content length for the clip".into())
    })?;
    if size < 16 {
        return Err(Error::Protocol("clip is suspiciously small".into()));
    }
    if !head.supports_ranges() {
        return Err(Error::NoRangeSupport);
    }

    let mut moov_at_end = false;
    let mut moov: Option<(BoxHeader, u64)> = None;
    let head_len = size.min(WINDOW);
    let head_buf = fetch_window(client, url, 0, head_len - 1)?;
    let head_boxes = scan_boxes_at(&head_buf, 0, Some(size))?;
    if let Some(found) = find_box(&head_boxes, b"moov") {
        if found.end() <= head_len {
            moov = Some((found, 0));
        }
    }

    if moov.is_none() && size > WINDOW {
        let tail_base = size - WINDOW;
        let tail_buf = fetch_window(client, url, tail_base, size - 1)?;
        let tail_boxes = scan_boxes_at(&tail_buf, tail_base, Some(size))?;
        if let Some(found) = find_box(&tail_boxes, b"moov") {
            if found.start >= tail_base && found.end() <= size {
                moov_at_end = true;
                moov = Some((found, tail_base));
            }
        }
    }

    if moov.is_none() && size > 2 * WINDOW {
        let mut offset = WINDOW;
        let mut windows = 0;
        while offset + 8 < size && windows < MAX_SCAN_WINDOWS {
            let end = (offset + WINDOW).min(size);
            let buf = fetch_window(client, url, offset, end - 1)?;
            let boxes = scan_boxes_at(&buf, offset, Some(size))?;
            if let Some(found) = find_box(&boxes, b"moov") {
                if found.end() <= size {
                    moov = Some((found, offset));
                }
                break;
            }
            offset = end;
            windows += 1;
        }
    }

    let (moov_header, moov_base) = moov.ok_or_else(|| Error::MoovNotFound(url.to_string()))?;

    let (moov_buf, base) = if moov_base == 0 && moov_header.end() <= head_len {
        let start = moov_header.start as usize;
        let end = start + moov_header.size as usize;
        (head_buf[start..end].to_vec(), 0u64)
    } else {
        fetch_box(client, url, &moov_header)?
    };

    let mut index = Mp4Index::parse(&moov_buf, base, size)?;
    let mut header_ranges = vec![(0u64, (size - 1).min(15))];
    if let Some(ftyp) = find_box(&head_boxes, b"ftyp") {
        header_ranges.push((ftyp.start, ftyp.end().saturating_sub(1)));
    }
    index.header_ranges = header_ranges;
    Ok(RemoteClipInfo {
        size,
        accepts_ranges: true,
        index,
        moov_at_end,
    })
}

fn unique_temp_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("gkuview-{tag}-{}-{nanos}.mp4", std::process::id()))
}

struct TempFile(PathBuf);

impl TempFile {
    fn new(tag: &str) -> Self {
        Self(unique_temp_path(tag))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn plan_for_clip(info: &RemoteClipInfo, start_s: f64, end_s: f64) -> Result<Plan> {
    plan::plan_window(&info.index, start_s, end_s)
}

fn range_fetch<'a>(
    client: &'a DashcamClient,
    url: &'a str,
) -> impl FnMut(u64, u64) -> Result<Vec<u8>> + 'a {
    move |start, end| {
        let (body, _) = client.get_range(url, start, end)?;
        Ok(body)
    }
}

fn crop_ts_clip(
    client: &DashcamClient,
    clip: &Clip,
    start_s: f64,
    end_s: f64,
    dest: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<CropReport> {
    let url = client.file_url(&clip.path);
    let head = client.head(&url)?;
    let size = head
        .total_size()
        .ok_or_else(|| Error::Protocol("device did not report a content length".into()))?;

    let temp = TempFile::new("clip");
    let stats: DownloadStats;
    let mut plan: Option<ts::TsPlan> = None;
    let mut fallback_reason: Option<String> = None;
    let mut used_range_requests = false;

    if head.supports_ranges() {
        let mut fetch = range_fetch(client, &url);
        match ts::plan(&mut fetch, size, start_s, end_s) {
            Ok(computed) => {
                let range = ByteRange::new(computed.range_start, computed.range_end);
                stats = crate::download::download_range_contiguous(
                    client,
                    &url,
                    range,
                    temp.path(),
                    progress,
                )?;
                used_range_requests = true;
                plan = Some(computed);
            }
            Err(Error::RangeUnsupported(_)) | Err(Error::NoRangeSupport) => {
                fallback_reason = Some("device ignored range requests".into());
                stats = download_full_to(client, &url, temp.path(), progress)?;
            }
            Err(e) => return Err(e),
        }
    } else {
        fallback_reason = Some("device does not support HTTP range requests".into());
        stats = download_full_to(client, &url, temp.path(), progress)?;
    }

    let (effective_start, effective_end, keyframe_start_s, keyframe_aligned) = match &plan {
        Some(plan) => (
            plan.start_s,
            plan.end_s,
            plan.keyframe_start_s,
            plan.keyframe_aligned,
        ),
        None => (start_s, end_s, 0.0, true),
    };

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let relative_start = (effective_start - keyframe_start_s).max(0.0);
    let relative_end = (effective_end - keyframe_start_s).max(relative_start + 0.05);
    ffmpeg::trim_copy(temp.path(), relative_start, relative_end, dest)?;
    let output_duration_s = ffmpeg::probe_duration(dest).ok();

    Ok(CropReport {
        output: dest.to_path_buf(),
        clip_name: clip.name.clone(),
        source_size: size,
        downloaded_bytes: stats.bytes_downloaded,
        requests: stats.requests,
        planned_bytes: plan
            .as_ref()
            .map(|p| p.planned_bytes())
            .unwrap_or(stats.bytes_downloaded),
        start_s: effective_start,
        end_s: effective_end,
        keyframe_start_s,
        keyframe_aligned,
        output_duration_s,
        used_range_requests,
        fallback_reason,
    })
}

pub fn crop_clip(
    client: &DashcamClient,
    clip: &Clip,
    start_s: f64,
    end_s: f64,
    dest: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<CropReport> {
    if !ffmpeg::available() {
        return Err(Error::FfmpegMissing);
    }
    if end_s <= start_s {
        return Err(Error::InvalidInput("end must be greater than start".into()));
    }
    if clip.is_mpeg_ts() {
        return crop_ts_clip(client, clip, start_s, end_s, dest, progress);
    }
    let url = client.file_url(&clip.path);

    let mut fallback_reason: Option<String> = None;
    let info = match locate_and_parse(client, &url) {
        Ok(info) => Some(info),
        Err(Error::NoRangeSupport) => {
            fallback_reason = Some("device does not support HTTP range requests".into());
            None
        }
        Err(Error::RangeUnsupported(_)) => {
            fallback_reason = Some("device ignored range requests".into());
            None
        }
        Err(Error::MoovNotFound(_)) => {
            fallback_reason = Some("moov atom could not be located remotely".into());
            None
        }
        Err(e) => return Err(e),
    };

    let temp = TempFile::new("clip");
    let stats: DownloadStats;
    let planned_bytes: u64;
    let used_range_requests: bool;
    let keyframe_start_s: f64;
    let keyframe_aligned: bool;
    let source_size: u64;
    let duration: f64;

    match info {
        Some(info) => {
            let plan = plan_for_clip(&info, start_s, end_s)?;
            duration = info.index.duration_secs();
            source_size = info.size;
            planned_bytes = plan.planned_bytes;
            keyframe_start_s = plan.keyframe_start_s;
            keyframe_aligned = plan.keyframe_aligned;
            match download_ranges_to(client, &url, &plan.ranges, temp.path(), progress) {
                Ok(s) => {
                    stats = s;
                    used_range_requests = true;
                }
                Err(Error::RangeUnsupported(_)) | Err(Error::NoRangeSupport) => {
                    fallback_reason = Some("device ignored range requests during download".into());
                    stats = download_full_to(client, &url, temp.path(), progress)?;
                    used_range_requests = false;
                }
                Err(e) => return Err(e),
            }
        }
        None => {
            stats = download_full_to(client, &url, temp.path(), progress)?;
            used_range_requests = false;
            planned_bytes = stats.bytes_downloaded;
            keyframe_start_s = start_s;
            keyframe_aligned = true;
            source_size = stats.bytes_downloaded;
            duration = 0.0;
        }
    }

    let effective_start = if duration > 0.0 {
        start_s.min(duration)
    } else {
        start_s
    };
    let effective_end = if duration > 0.0 {
        end_s.min(duration)
    } else {
        end_s
    };

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ffmpeg::trim_copy(temp.path(), effective_start, effective_end, dest)?;
    let output_duration_s = ffmpeg::probe_duration(dest).ok();

    Ok(CropReport {
        output: dest.to_path_buf(),
        clip_name: clip.name.clone(),
        source_size,
        downloaded_bytes: stats.bytes_downloaded,
        requests: stats.requests,
        planned_bytes,
        start_s: effective_start,
        end_s: effective_end,
        keyframe_start_s,
        keyframe_aligned,
        output_duration_s,
        used_range_requests,
        fallback_reason,
    })
}

pub fn preview_frame(
    client: &DashcamClient,
    clip: &Clip,
    at_s: f64,
    dest_png: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    if !ffmpeg::available() {
        return Err(Error::FfmpegMissing);
    }
    let url = client.file_url(&clip.path);

    if clip.is_mpeg_ts() {
        let head = client.head(&url)?;
        let size = head
            .total_size()
            .ok_or_else(|| Error::Protocol("device did not report a content length".into()))?;
        let mut fetch = range_fetch(client, &url);
        let start = (at_s - 0.5).max(0.0);
        let end = start + 1.0;
        let plan = ts::plan(&mut fetch, size, start, end)?;
        let range = ByteRange::new(plan.range_start, plan.range_end);
        let temp = TempFile::new("preview");
        crate::download::download_range_contiguous(client, &url, range, temp.path(), progress)?;
        if let Some(parent) = dest_png.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let relative = (plan.start_s.min(at_s) - plan.keyframe_start_s).max(0.0);
        return ffmpeg::extract_frame(temp.path(), relative, dest_png);
    }

    let info = locate_and_parse(client, &url)?;
    let duration = info.index.duration_secs();
    let at_s = at_s.clamp(0.0, (duration - 0.05).max(0.0));
    let start = (at_s - 0.5).max(0.0);
    let end = (start + 1.0).min(duration.max(start + 0.1));
    let plan = plan_for_clip(&info, start, end)?;

    let temp = TempFile::new("preview");
    download_ranges_to(client, &url, &plan.ranges, temp.path(), progress)?;
    if let Some(parent) = dest_png.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ffmpeg::extract_frame(temp.path(), at_s, dest_png)
}

pub fn clip_duration(client: &DashcamClient, clip: &Clip) -> Result<f64> {
    let url = client.file_url(&clip.path);
    if clip.is_mpeg_ts() {
        let head = client.head(&url)?;
        let size = head
            .total_size()
            .ok_or_else(|| Error::Protocol("device did not report a content length".into()))?;
        let mut fetch = range_fetch(client, &url);
        return ts::duration(&mut fetch, size);
    }
    let info = locate_and_parse(client, &url)?;
    Ok(info.index.duration_secs())
}
