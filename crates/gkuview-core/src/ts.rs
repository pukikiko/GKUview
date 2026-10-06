use crate::error::{Error, Result};

pub const PACKET_SIZE: usize = 188;
const SYNC: u8 = 0x47;
pub const PTS_HZ: f64 = 90_000.0;
const SEARCH_WINDOW: u64 = 64 * 1024;
const PROBE_WINDOW: u64 = 96 * 1024;
const KEYFRAME_CHUNK: u64 = 1024 * 1024;
const MAX_KEYFRAME_LOOKBACK: u64 = 32 * 1024 * 1024;
const WRAP: i64 = 1 << 33;
const HALF_WRAP: i64 = 1 << 32;

/// A range-fetch callback: `(start, end_inclusive) -> bytes`.
pub type Fetch<'a> = dyn FnMut(u64, u64) -> Result<Vec<u8>> + 'a;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtsEntry {
    pub offset: u64,
    pub pts: u64,
    pub random_access: bool,
    pub pid: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct TsInfo {
    pub size: u64,
    pub first_offset: u64,
    pub first_pts: u64,
    pub last_pts: u64,
    pub duration_s: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Before,
    After,
}

#[derive(Debug, Clone)]
pub struct TsPlan {
    pub range_start: u64,
    pub range_end: u64,
    pub keyframe_offset: u64,
    pub keyframe_pts: u64,
    pub keyframe_found: bool,
    pub first_pts: u64,
    pub duration_s: f64,
    pub start_s: f64,
    pub end_s: f64,
    pub keyframe_start_s: f64,
    pub keyframe_aligned: bool,
    pub requests: usize,
}

impl TsPlan {
    pub fn planned_bytes(&self) -> u64 {
        self.range_end.saturating_sub(self.range_start) + 1
    }
}

fn sync_lattice(buf: &[u8]) -> Option<(usize, usize)> {
    for size in [PACKET_SIZE, 192, 204] {
        if buf.len() < size * 2 {
            continue;
        }
        for off in 0..size.min(buf.len()) {
            if buf[off] == SYNC && buf.get(off + size) == Some(&SYNC) {
                let third_ok = match buf.get(off + 2 * size) {
                    Some(b) => *b == SYNC,
                    None => true,
                };
                if third_ok {
                    return Some((off, size));
                }
            }
        }
    }
    None
}

fn read_pts(b: &[u8]) -> Option<u64> {
    if b.len() < 5 {
        return None;
    }
    Some(
        (((b[0] >> 1) & 0x07) as u64) << 30
            | (b[1] as u64) << 22
            | (((b[2] >> 1) & 0x7F) as u64) << 15
            | (b[3] as u64) << 7
            | ((b[4] >> 1) & 0x7F) as u64,
    )
}

struct PacketInfo {
    pid: u16,
    pusi: bool,
    has_payload: bool,
    random_access: bool,
    payload_start: usize,
    payload_end: usize,
}

fn parse_packet_header(buf: &[u8], off: usize) -> Option<PacketInfo> {
    if off + 4 > buf.len() || buf[off] != SYNC {
        return None;
    }
    let b1 = buf[off + 1];
    let pid = ((b1 & 0x1F) as u16) << 8 | buf[off + 2] as u16;
    let pusi = b1 & 0x40 != 0;
    let afc = (buf[off + 3] >> 4) & 0x03;
    let mut p = off + 4;
    let mut random_access = false;
    if afc & 0x02 != 0 {
        let afl = *buf.get(p)? as usize;
        if afl > 0 {
            random_access = buf.get(p + 1).map(|b| b & 0x40 != 0).unwrap_or(false);
        }
        p += 1 + afl;
    }
    Some(PacketInfo {
        pid,
        pusi,
        has_payload: afc & 0x01 != 0,
        random_access,
        payload_start: p,
        payload_end: (off + PACKET_SIZE).min(buf.len()),
    })
}

/// True when an Annex-B elementary stream chunk contains a random access
/// picture: H.264 IDR/SPS or H.265 IRAP/VPS. Many dashcams (including the GKU
/// D600Pro) never set the TS adaptation-field random access indicator, so this
/// is the reliable signal.
pub fn contains_keyframe_nal(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let b = data[i + 3];
            let h264_type = b & 0x1F;
            let hevc_type = (b >> 1) & 0x3F;
            if h264_type == 5
                || h264_type == 7
                || (16..=21).contains(&hevc_type)
                || hevc_type == 32
                || hevc_type == 33
            {
                return true;
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    false
}

struct PendingPes {
    entry_index: usize,
    pid: u16,
    data: Vec<u8>,
}

const PES_SCAN_BYTES: usize = 4096;

pub fn parse_pts_entries(buf: &[u8], base: u64) -> Vec<PtsEntry> {
    let mut out: Vec<PtsEntry> = Vec::new();
    let Some((mut off, size)) = sync_lattice(buf) else {
        return out;
    };
    let mut pending: Option<PendingPes> = None;

    while off + 4 <= buf.len() {
        if buf[off] != SYNC {
            off += 1;
            continue;
        }
        if let Some(info) = parse_packet_header(buf, off) {
            let video_pes = info.pusi
                && info.has_payload
                && info.pid != 0
                && info.pid != 0x1FFF
                && info.payload_start + 14 <= info.payload_end
                && buf[info.payload_start..info.payload_start + 3] == [0, 0, 1]
                && (0xE0..=0xEF).contains(&buf[info.payload_start + 3])
                && buf[info.payload_start + 7] & 0x80 != 0;

            if video_pes {
                if let Some(pd) = pending.take() {
                    if contains_keyframe_nal(&pd.data) {
                        out[pd.entry_index].random_access = true;
                    }
                }
                if let Some(pts) = read_pts(&buf[info.payload_start + 9..info.payload_start + 14]) {
                    out.push(PtsEntry {
                        offset: base + off as u64,
                        pts,
                        random_access: info.random_access,
                        pid: info.pid,
                    });
                    pending = Some(PendingPes {
                        entry_index: out.len() - 1,
                        pid: info.pid,
                        data: buf[info.payload_start..info.payload_end].to_vec(),
                    });
                }
            } else if let Some(pd) = pending.as_mut() {
                if pd.pid == info.pid && info.has_payload && pd.data.len() < PES_SCAN_BYTES {
                    let remain = PES_SCAN_BYTES - pd.data.len();
                    let start = info.payload_start;
                    let end = (start + remain).min(info.payload_end);
                    if end > start {
                        pd.data.extend_from_slice(&buf[start..end]);
                    }
                }
            }
        }
        off += size;
    }

    if let Some(pd) = pending.take() {
        if contains_keyframe_nal(&pd.data) {
            out[pd.entry_index].random_access = true;
        }
    }
    out
}

pub fn unwrap_near(pts: u64, reference: i64) -> i64 {
    let mut v = pts as i64;
    while v - reference > HALF_WRAP {
        v -= WRAP;
    }
    while reference - v > HALF_WRAP {
        v += WRAP;
    }
    v
}

fn fetch_window(fetch: &mut Fetch, start: u64, end: u64) -> Result<Vec<u8>> {
    if end < start {
        return Err(Error::InvalidInput(format!(
            "invalid fetch window {start}..{end}"
        )));
    }
    fetch(start, end)
}

pub fn probe(fetch: &mut Fetch, size: u64) -> Result<TsInfo> {
    if size < (PACKET_SIZE * 2) as u64 {
        return Err(Error::Protocol(
            "file too small to be an MPEG-TS recording".into(),
        ));
    }
    let head_end = PROBE_WINDOW.min(size) - 1;
    let head = fetch_window(fetch, 0, head_end)?;
    let entries = parse_pts_entries(&head, 0);
    let first = *entries.first().ok_or_else(|| {
        Error::Protocol("no video PES timestamps found; not an MPEG-TS recording?".into())
    })?;

    let mut last: Option<PtsEntry> = None;
    let mut window = PROBE_WINDOW.min(size);
    for attempt in 0..8u64 {
        let start = size.saturating_sub(window * (attempt + 1));
        let end = (start + window - 1).min(size - 1);
        let buf = fetch_window(fetch, start, end)?;
        let entries = parse_pts_entries(&buf, start);
        if let Some(entry) = entries.last() {
            last = Some(*entry);
            break;
        }
        if start == 0 {
            break;
        }
        window = window.max(PROBE_WINDOW) * 2;
    }
    let last =
        last.ok_or_else(|| Error::Protocol("no video PES timestamps near end of file".into()))?;

    let duration_ticks = (unwrap_near(last.pts, first.pts as i64) - first.pts as i64).max(0);
    Ok(TsInfo {
        size,
        first_offset: first.offset,
        first_pts: first.pts,
        last_pts: last.pts,
        duration_s: duration_ticks as f64 / PTS_HZ,
    })
}

pub fn find_position(
    fetch: &mut Fetch,
    info: &TsInfo,
    target_s: f64,
    mode: Position,
) -> Result<(PtsEntry, usize)> {
    let target = info.first_pts as i64 + (target_s.max(0.0) * PTS_HZ).round() as i64;
    let mut lo: u64 = 0;
    let mut hi: u64 = info.size;
    let mut requests = 0usize;

    while hi.saturating_sub(lo) > SEARCH_WINDOW {
        let mid = lo + (hi - lo) / 2;
        let end = (mid + SEARCH_WINDOW - 1).min(info.size - 1);
        let buf = fetch_window(fetch, mid, end)?;
        requests += 1;
        let entries = parse_pts_entries(&buf, mid);
        let Some(first) = entries.first() else {
            hi = mid;
            continue;
        };
        let pts = unwrap_near(first.pts, info.first_pts as i64);
        if pts < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    let end = (lo + 2 * SEARCH_WINDOW).min(info.size).saturating_sub(1);
    let buf = fetch_window(fetch, lo, end)?;
    requests += 1;
    let entries = parse_pts_entries(&buf, lo);
    if entries.is_empty() {
        return Err(Error::Protocol(
            "no video PES timestamps near the requested position".into(),
        ));
    }

    let chosen = match mode {
        Position::Before => {
            let mut chosen = entries[0];
            for entry in &entries {
                if unwrap_near(entry.pts, info.first_pts as i64) <= target {
                    chosen = *entry;
                }
            }
            chosen
        }
        Position::After => {
            let mut chosen = *entries.last().unwrap();
            for entry in &entries {
                if unwrap_near(entry.pts, info.first_pts as i64) >= target {
                    chosen = *entry;
                    break;
                }
            }
            chosen
        }
    };
    Ok((chosen, requests))
}

/// Finds the last random-access point at or before `before_offset`.
pub fn find_keyframe(
    fetch: &mut Fetch,
    info: &TsInfo,
    before_offset: u64,
) -> Result<(PtsEntry, bool, usize)> {
    let floor = before_offset.saturating_sub(MAX_KEYFRAME_LOOKBACK);
    let mut cursor = before_offset.saturating_add(PACKET_SIZE as u64);
    let mut earliest: Option<PtsEntry> = None;
    let mut requests = 0usize;

    while cursor > floor {
        let start = cursor.saturating_sub(KEYFRAME_CHUNK).max(floor);
        let end = (cursor + PACKET_SIZE as u64)
            .min(info.size)
            .saturating_sub(1);
        let buf = fetch_window(fetch, start, end)?;
        requests += 1;
        let entries = parse_pts_entries(&buf, start);
        if let Some(entry) = entries.first() {
            earliest = Some(*entry);
        }
        if let Some(kf) = entries
            .iter()
            .rev()
            .find(|e| e.random_access && e.offset <= before_offset)
        {
            return Ok((*kf, true, requests));
        }
        cursor = start;
    }

    let fallback = earliest.unwrap_or(PtsEntry {
        offset: info.first_offset,
        pts: info.first_pts,
        random_access: false,
        pid: 0,
    });
    Ok((fallback, false, requests))
}

pub fn plan(fetch: &mut Fetch, size: u64, start_s: f64, end_s: f64) -> Result<TsPlan> {
    if !start_s.is_finite() || !end_s.is_finite() {
        return Err(Error::InvalidInput("start/end must be finite".into()));
    }
    if start_s < 0.0 || end_s <= start_s {
        return Err(Error::InvalidInput("end must be greater than start".into()));
    }

    let info = probe(fetch, size)?;
    let duration = info.duration_s;
    if duration <= 0.0 {
        return Err(Error::Protocol(
            "could not determine TS clip duration".into(),
        ));
    }
    let start_s = start_s.min(duration);
    let end_s = end_s.min(duration);
    if end_s <= start_s {
        return Err(Error::InvalidInput(format!(
            "selection is outside the clip (duration {duration:.2}s)"
        )));
    }

    let (start_entry, r1) = find_position(fetch, &info, start_s, Position::Before)?;
    let (end_entry, r2) = find_position(fetch, &info, end_s, Position::After)?;
    let (keyframe, keyframe_found, r3) = find_keyframe(fetch, &info, start_entry.offset)?;

    let range_start = keyframe.offset.min(start_entry.offset);
    let range_end = (end_entry.offset + PACKET_SIZE as u64)
        .min(size)
        .saturating_sub(1);
    if range_end <= range_start {
        return Err(Error::Protocol("computed an empty byte range".into()));
    }

    let keyframe_start_s =
        (unwrap_near(keyframe.pts, info.first_pts as i64) - info.first_pts as i64) as f64 / PTS_HZ;

    Ok(TsPlan {
        range_start,
        range_end,
        keyframe_offset: keyframe.offset,
        keyframe_pts: keyframe.pts,
        keyframe_found,
        first_pts: info.first_pts,
        duration_s: duration,
        start_s,
        end_s,
        keyframe_start_s,
        keyframe_aligned: (keyframe_start_s - start_s).abs() < 0.05,
        requests: r1 + r2 + r3,
    })
}

pub fn duration(fetch: &mut Fetch, size: u64) -> Result<f64> {
    Ok(probe(fetch, size)?.duration_s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_pts(pts: u64) -> [u8; 5] {
        [
            0x21 | (((pts >> 30) & 0x07) as u8) << 1,
            (pts >> 22) as u8,
            0x01 | (((pts >> 15) & 0x7F) as u8) << 1,
            (pts >> 7) as u8,
            0x01 | ((pts & 0x7F) as u8) << 1,
        ]
    }

    fn pes_packet(pid: u16, pts: u64, random_access: bool, counter: u8) -> [u8; PACKET_SIZE] {
        let mut pkt = [0xFFu8; PACKET_SIZE];
        pkt[0] = SYNC;
        pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x30;
        pkt[4] = 7;
        pkt[5] = if random_access { 0x40 } else { 0x00 };
        pkt[6] = 0;
        pkt[7] = 0;
        pkt[8] = 0;
        pkt[9] = 0;
        pkt[10] = 0;
        pkt[11] = 0;
        pkt[12] = 0;
        let p = 12;
        pkt[p] = 0;
        pkt[p + 1] = 0;
        pkt[p + 2] = 1;
        pkt[p + 3] = 0xE0;
        pkt[p + 4] = 0;
        pkt[p + 5] = 0;
        pkt[p + 6] = 0x80;
        pkt[p + 7] = 0x80;
        pkt[p + 8] = 5;
        pkt[p + 9..p + 14].copy_from_slice(&encode_pts(pts));
        pkt[p + 14] = counter.wrapping_mul(17);
        pkt
    }

    fn make_stream(frames: usize, pts_start: u64, gop: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..frames {
            let pts = pts_start + (i as u64) * 9000;
            out.extend_from_slice(&pes_packet(0x100, pts, i % gop == 0, (i % 251) as u8));
        }
        out
    }

    #[test]
    fn parses_synthetic_pts() {
        let stream = make_stream(30, 0, 10);
        let entries = parse_pts_entries(&stream, 0);
        assert_eq!(entries.len(), 30);
        assert_eq!(entries[0].pts, 0);
        assert_eq!(entries[1].pts, 9000);
        assert!(entries[0].random_access);
        assert!(!entries[5].random_access);
        assert!(entries[10].random_access);
    }

    #[test]
    fn lattice_tolerates_mid_packet_start() {
        let stream = make_stream(10, 0, 10);
        let offset = 60;
        let entries = parse_pts_entries(&stream[offset..], 0);
        assert!(!entries.is_empty());
        assert_eq!(entries[0].offset, (PACKET_SIZE - offset) as u64);
    }

    #[test]
    fn detects_nal_keyframes() {
        assert!(contains_keyframe_nal(&[0, 0, 1, 0x26, 1, 2, 3])); // HEVC IDR_W_RADL
        assert!(contains_keyframe_nal(&[0, 0, 0, 1, 0x40, 1])); // HEVC VPS
        assert!(contains_keyframe_nal(&[0, 0, 1, 0x65, 1])); // H.264 IDR
        assert!(contains_keyframe_nal(&[0, 0, 1, 0x67, 1])); // H.264 SPS
        assert!(!contains_keyframe_nal(&[0, 0, 1, 0x02, 1, 2])); // HEVC TRAIL_R
        assert!(!contains_keyframe_nal(&[0x47, 0x00, 0x10, 0x19]));
    }

    #[test]
    fn detects_pes_keyframe_from_nal_without_rai() {
        let mut first = pes_packet(0x100, 9000, false, 0);
        let p = 12 + 14;
        first[p] = 0;
        first[p + 1] = 0;
        first[p + 2] = 0;
        first[p + 3] = 1;
        first[p + 4] = 0x26;
        let mut stream = first.to_vec();
        stream.extend_from_slice(&pes_packet(0x100, 18000, false, 1));
        let entries = parse_pts_entries(&stream, 0);
        assert_eq!(entries.len(), 2);
        assert!(
            entries[0].random_access,
            "IDR NAL should mark the PES as a keyframe"
        );
        assert!(!entries[1].random_access);
    }

    #[test]
    fn unwrap_handles_wraparound() {
        let near_wrap = WRAP as u64 - 100;
        let after_wrap = 50u64;
        assert_eq!(unwrap_near(after_wrap, near_wrap as i64), WRAP + 50);
    }

    #[test]
    fn probe_position_and_keyframe() {
        let stream = make_stream(300, 1000, 10);
        let size = stream.len() as u64;
        let mut fetch = |start: u64, end: u64| -> Result<Vec<u8>> {
            Ok(stream[start as usize..=end as usize].to_vec())
        };
        let info = probe(&mut fetch, size).unwrap();
        assert!((info.duration_s - 29.9).abs() < 0.1);

        let (entry, _) = find_position(&mut fetch, &info, 10.0, Position::Before).unwrap();
        let rel = unwrap_near(entry.pts, info.first_pts as i64) - info.first_pts as i64;
        assert!(rel as f64 <= 10.0 * PTS_HZ);
        assert!(rel as f64 > 9.9 * PTS_HZ);

        let plan = plan(&mut fetch, size, 10.0, 12.0).unwrap();
        assert!(plan.keyframe_found);
        assert!((plan.keyframe_start_s - 10.0).abs() < 0.1);
        assert!(plan.planned_bytes() < size);
        assert!(plan.range_end < size);
    }
}
