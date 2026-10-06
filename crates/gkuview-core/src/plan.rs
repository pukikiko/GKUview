use crate::error::{Error, Result};
use crate::mp4::{Mp4Index, Track};

pub const DEFAULT_MERGE_GAP: u64 = 32 * 1024;
const MAX_RANGES: usize = 512;
const RANGE_PAD: u64 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start) + 1
    }

    pub fn is_empty(&self) -> bool {
        self.end < self.start
    }

    pub fn header_value(&self) -> String {
        format!("bytes={}-{}", self.start, self.end)
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub ranges: Vec<ByteRange>,
    pub window_start_s: f64,
    pub window_end_s: f64,
    pub keyframe_start_s: f64,
    pub keyframe_aligned: bool,
    pub planned_bytes: u64,
    pub span_bytes: u64,
    pub source_size: u64,
    pub collapsed: bool,
}

impl Plan {
    pub fn saved_percent(&self) -> f64 {
        if self.source_size == 0 {
            return 0.0;
        }
        (1.0 - self.planned_bytes as f64 / self.source_size as f64) * 100.0
    }

    pub fn request_count(&self) -> usize {
        self.ranges.len()
    }
}

fn find_keyframe_start(track: &Track, start_s: f64) -> (usize, bool) {
    let ts = start_s * track.timescale as f64;
    let mut candidate: Option<usize> = None;
    let mut first_sync: Option<usize> = None;
    for (i, sample) in track.samples.iter().enumerate() {
        if sample.sync && first_sync.is_none() {
            first_sync = Some(i);
        }
        if sample.sync && sample.pts as f64 <= ts {
            candidate = Some(i);
        }
    }
    match candidate {
        Some(i) => {
            let aligned = track.samples[i].pts_secs(track.timescale) >= start_s - 0.001;
            (i, aligned)
        }
        None => (first_sync.unwrap_or(0), false),
    }
}

fn collect_track_ranges(track: &Track, start_sample: usize, end_s: f64, out: &mut Vec<(u64, u64)>) {
    let end_ts = (end_s * track.timescale as f64).ceil() as u64 + 1;
    for (i, sample) in track.samples.iter().enumerate() {
        if i < start_sample || sample.pts > end_ts {
            continue;
        }
        let start = sample.offset.saturating_sub(RANGE_PAD);
        let end = sample.end().saturating_add(RANGE_PAD);
        out.push((start, end.max(start)));
    }
}

fn merge_ranges(mut raw: Vec<(u64, u64)>, merge_gap: u64) -> Vec<ByteRange> {
    raw.sort_unstable();
    let mut merged: Vec<ByteRange> = Vec::new();
    for (start, end) in raw {
        match merged.last_mut() {
            Some(last) if start <= last.end.saturating_add(merge_gap).saturating_add(1) => {
                last.end = last.end.max(end);
            }
            _ => merged.push(ByteRange::new(start, end)),
        }
    }
    merged
}

pub fn plan_window(index: &Mp4Index, start_s: f64, end_s: f64) -> Result<Plan> {
    plan_window_with_gap(index, start_s, end_s, DEFAULT_MERGE_GAP)
}

pub fn plan_window_with_gap(
    index: &Mp4Index,
    start_s: f64,
    end_s: f64,
    merge_gap: u64,
) -> Result<Plan> {
    if index.fragmented {
        return Err(Error::Protocol(
            "this clip uses fragmented MP4 (moof); partial range download is not supported".into(),
        ));
    }
    if !start_s.is_finite() || !end_s.is_finite() {
        return Err(Error::InvalidInput("start/end must be finite".into()));
    }
    if start_s < 0.0 {
        return Err(Error::InvalidInput("start must be >= 0".into()));
    }
    if end_s <= start_s {
        return Err(Error::InvalidInput("end must be greater than start".into()));
    }
    let duration = index.duration_secs();
    let start_s = start_s.max(0.0);
    let end_s = if duration > 0.0 {
        end_s.min(duration)
    } else {
        end_s
    };
    if end_s <= start_s {
        return Err(Error::InvalidInput(format!(
            "end ({end_s:.3}s) is beyond the clip duration ({duration:.3}s)"
        )));
    }

    let video = index.video_track();
    let (start_sample, keyframe_start_s, aligned) = match video {
        Some(track) => {
            let (sample, aligned) = find_keyframe_start(track, start_s);
            let kf = track
                .samples
                .get(sample)
                .map_or(start_s, |s| s.pts_secs(track.timescale));
            (sample, kf, aligned)
        }
        None => (0, start_s, true),
    };

    let mut raw: Vec<(u64, u64)> = Vec::new();
    if let Some(track) = video {
        collect_track_ranges(track, start_sample, end_s, &mut raw);
    }
    for track in &index.tracks {
        if track.is_video() {
            continue;
        }
        let audio_preroll = if track.is_audio() { 1.0 } else { 0.0 };
        let ts = ((start_s - audio_preroll).max(0.0) * track.timescale as f64) as u64;
        let first = track.samples.iter().position(|s| s.pts >= ts).unwrap_or(0);
        collect_track_ranges(track, first, end_s, &mut raw);
    }

    raw.push((
        index.moov_start,
        index.moov_end.saturating_sub(1).max(index.moov_start),
    ));
    for (start, end) in &index.header_ranges {
        raw.push((*start, *end));
    }

    if raw.is_empty() {
        return Err(Error::InvalidInput(
            "no samples found in selected window".into(),
        ));
    }

    let mut merged = merge_ranges(raw, merge_gap);
    let mut collapsed = false;

    let planned: u64 = merged.iter().map(|r| r.len()).sum();
    let span = merged
        .last()
        .map(|l| l.end)
        .unwrap_or(0)
        .saturating_sub(merged.first().map(|f| f.start).unwrap_or(0))
        + 1;

    if merged.len() > MAX_RANGES || (span <= planned.saturating_mul(5) / 2 && merged.len() > 1) {
        let first = merged.first().unwrap().start;
        let last = merged.last().unwrap().end;
        merged = vec![ByteRange::new(first, last)];
        collapsed = true;
    }
    if planned >= index.file_size.saturating_mul(9) / 10 && index.file_size > 0 {
        merged = vec![ByteRange::new(0, index.file_size.saturating_sub(1))];
        collapsed = true;
    }

    let planned: u64 = merged.iter().map(|r| r.len()).sum();

    Ok(Plan {
        ranges: merged,
        window_start_s: keyframe_start_s,
        window_end_s: end_s,
        keyframe_start_s,
        keyframe_aligned: aligned && (keyframe_start_s - start_s).abs() < 0.05,
        planned_bytes: planned,
        span_bytes: span,
        source_size: index.file_size,
        collapsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merging() {
        let merged = merge_ranges(vec![(100, 199), (200, 300), (10_000, 10_100)], 0);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], ByteRange::new(100, 300));
    }

    #[test]
    fn merging_with_gap() {
        let merged = merge_ranges(vec![(100, 199), (250, 300)], 64);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0], ByteRange::new(100, 300));
    }

    #[test]
    fn header_value_format() {
        assert_eq!(ByteRange::new(5, 10).header_value(), "bytes=5-10");
    }
}
