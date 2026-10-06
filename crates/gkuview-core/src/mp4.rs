use crate::error::{Error, Result};

pub type FourCC = [u8; 4];

pub fn fourcc(s: &[u8; 4]) -> String {
    String::from_utf8_lossy(s).to_string()
}

#[derive(Debug, Clone, Copy)]
pub struct BoxHeader {
    pub kind: FourCC,
    pub start: u64,
    pub header_len: u64,
    pub size: u64,
}

impl BoxHeader {
    pub fn payload_start(&self) -> usize {
        self.header_len as usize
    }

    pub fn end(&self) -> u64 {
        self.start + self.size
    }
}

fn read_u32(buf: &[u8], off: usize) -> Result<u32> {
    let bytes = buf
        .get(off..off + 4)
        .ok_or_else(|| Error::Mp4(format!("truncated u32 at {off}")))?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(buf: &[u8], off: usize) -> Result<u64> {
    let bytes = buf
        .get(off..off + 8)
        .ok_or_else(|| Error::Mp4(format!("truncated u64 at {off}")))?;
    Ok(u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

pub fn parse_box_header(
    buf: &[u8],
    pos: usize,
    file_size: Option<u64>,
) -> Result<Option<(BoxHeader, usize)>> {
    if pos + 8 > buf.len() {
        return Ok(None);
    }
    let size32 = read_u32(buf, pos)?;
    let kind: FourCC = buf[pos + 4..pos + 8].try_into().unwrap();
    let (size, header_len) = if size32 == 1 {
        if pos + 16 > buf.len() {
            return Err(Error::Mp4("truncated 64-bit box header".into()));
        }
        (read_u64(buf, pos + 8)?, 16u64)
    } else if size32 == 0 {
        let end = file_size.unwrap_or((buf.len() - pos) as u64);
        (end.saturating_sub(pos as u64), 8u64)
    } else {
        (size32 as u64, 8u64)
    };
    if size < header_len {
        return Err(Error::Mp4(format!(
            "invalid box {} size {size}",
            fourcc(&kind)
        )));
    }
    let next = pos + size as usize;
    Ok(Some((
        BoxHeader {
            kind,
            start: pos as u64,
            header_len,
            size,
        },
        next,
    )))
}

pub fn scan_boxes(buf: &[u8], file_size: Option<u64>) -> Result<Vec<BoxHeader>> {
    scan_boxes_at(buf, 0, file_size)
}

pub fn scan_boxes_at(buf: &[u8], base: u64, file_size: Option<u64>) -> Result<Vec<BoxHeader>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some((mut header, next)) = parse_box_header(buf, pos, file_size)? {
        header.start += base;
        out.push(header);
        if next <= pos {
            break;
        }
        pos = next;
    }
    Ok(out)
}

pub fn find_child(buf: &[u8], kind: &FourCC) -> Option<BoxHeader> {
    scan_boxes(buf, None)
        .ok()?
        .into_iter()
        .find(|b| &b.kind == kind)
}

fn payload<'a>(buf: &'a [u8], header: &BoxHeader) -> Result<&'a [u8]> {
    let start = header.start as usize + header.header_len as usize;
    let end = ((header.start + header.size) as usize).min(buf.len());
    buf.get(start..end)
        .ok_or_else(|| Error::Mp4("box payload out of range".into()))
}

#[derive(Debug, Clone)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    pub dts: u64,
    pub dur: u32,
    pub pts: u64,
    pub sync: bool,
}

impl Sample {
    pub fn end(&self) -> u64 {
        self.offset + self.size as u64
    }

    pub fn dts_secs(&self, timescale: u32) -> f64 {
        self.dts as f64 / timescale as f64
    }

    pub fn pts_secs(&self, timescale: u32) -> f64 {
        self.pts as f64 / timescale as f64
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    pub id: u32,
    pub handler: FourCC,
    pub timescale: u32,
    pub duration: u64,
    pub codec: FourCC,
    pub samples: Vec<Sample>,
}

impl Track {
    pub fn duration_secs(&self) -> f64 {
        if self.timescale == 0 {
            0.0
        } else {
            self.duration as f64 / self.timescale as f64
        }
    }

    pub fn is_video(&self) -> bool {
        &self.handler == b"vide"
    }

    pub fn is_audio(&self) -> bool {
        &self.handler == b"soun"
    }

    pub fn last_sample_end_secs(&self) -> f64 {
        self.samples
            .iter()
            .map(|s| (s.pts + s.dur as u64) as f64 / self.timescale.max(1) as f64)
            .fold(0.0, f64::max)
    }
}

#[derive(Debug, Clone)]
pub struct Mp4Index {
    pub timescale: u32,
    pub duration: u64,
    pub tracks: Vec<Track>,
    pub fragmented: bool,
    pub moov_start: u64,
    pub moov_end: u64,
    pub file_size: u64,
    pub header_ranges: Vec<(u64, u64)>,
}

impl Mp4Index {
    pub fn duration_secs(&self) -> f64 {
        if let Some(video) = self.video_track() {
            let last = video.last_sample_end_secs();
            if last > 0.0 {
                return last;
            }
        }
        if self.timescale == 0 {
            0.0
        } else {
            self.duration as f64 / self.timescale as f64
        }
    }

    pub fn video_track(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.is_video())
    }

    pub fn audio_track(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.is_audio())
    }
}

fn sub_boxes(container: &[u8]) -> Result<Vec<(BoxHeader, &[u8])>> {
    let boxes = scan_boxes(container, None)?;
    Ok(boxes
        .into_iter()
        .filter_map(|b| {
            let p = payload(container, &b).ok()?;
            Some((b, p))
        })
        .collect())
}

fn parse_mvhd(body: &[u8]) -> Result<(u32, u64)> {
    let version = *body
        .first()
        .ok_or_else(|| Error::Mp4("empty mvhd".into()))?;
    let (timescale, duration) = if version == 1 {
        (read_u32(body, 20)?, read_u64(body, 24)?)
    } else {
        (read_u32(body, 12)?, read_u32(body, 16)? as u64)
    };
    Ok((timescale, duration))
}

fn parse_tkhd(body: &[u8]) -> Result<u32> {
    let version = *body
        .first()
        .ok_or_else(|| Error::Mp4("empty tkhd".into()))?;
    let track_id = if version == 1 {
        read_u32(body, 20)?
    } else {
        read_u32(body, 12)?
    };
    Ok(track_id)
}

fn parse_mdhd(body: &[u8]) -> Result<(u32, u64)> {
    let version = *body
        .first()
        .ok_or_else(|| Error::Mp4("empty mdhd".into()))?;
    let (timescale, duration) = if version == 1 {
        (read_u32(body, 20)?, read_u64(body, 24)?)
    } else {
        (read_u32(body, 12)?, read_u32(body, 16)? as u64)
    };
    Ok((timescale, duration))
}

fn parse_hdlr(body: &[u8]) -> Result<FourCC> {
    let h = body
        .get(8..12)
        .ok_or_else(|| Error::Mp4("short hdlr".into()))?;
    Ok(h.try_into().unwrap())
}

fn parse_stsd_codec(body: &[u8]) -> Result<FourCC> {
    let h = body
        .get(12..16)
        .ok_or_else(|| Error::Mp4("short stsd".into()))?;
    Ok(h.try_into().unwrap())
}

fn parse_stts(body: &[u8]) -> Result<Vec<(u32, u32)>> {
    let count = read_u32(body, 4)? as usize;
    let mut out = Vec::with_capacity(count.min(1 << 20));
    for i in 0..count {
        let off = 8 + i * 8;
        out.push((read_u32(body, off)?, read_u32(body, off + 4)?));
    }
    Ok(out)
}

fn parse_ctts(body: &[u8]) -> Result<Vec<(u32, i64)>> {
    let count = read_u32(body, 4)? as usize;
    let mut out = Vec::with_capacity(count.min(1 << 20));
    for i in 0..count {
        let off = 8 + i * 8;
        let offset = read_u32(body, off + 4)? as i32 as i64;
        out.push((read_u32(body, off)?, offset));
    }
    Ok(out)
}

fn parse_stsc(body: &[u8]) -> Result<Vec<(u32, u32, u32)>> {
    let count = read_u32(body, 4)? as usize;
    let mut out = Vec::with_capacity(count.min(1 << 20));
    for i in 0..count {
        let off = 8 + i * 12;
        out.push((
            read_u32(body, off)?,
            read_u32(body, off + 4)?,
            read_u32(body, off + 8)?,
        ));
    }
    Ok(out)
}

fn parse_stsz(body: &[u8]) -> Result<(u32, usize, Vec<u32>)> {
    let fixed = read_u32(body, 4)?;
    let count = read_u32(body, 8)? as usize;
    if fixed != 0 {
        return Ok((fixed, count, Vec::new()));
    }
    if count > 4_000_000 {
        return Err(Error::Mp4(format!("unreasonable sample count {count}")));
    }
    let mut sizes = Vec::with_capacity(count);
    for i in 0..count {
        sizes.push(read_u32(body, 12 + i * 4)?);
    }
    Ok((fixed, count, sizes))
}

fn parse_stco(body: &[u8], wide: bool) -> Result<Vec<u64>> {
    let count = read_u32(body, 4)? as usize;
    if count > 4_000_000 {
        return Err(Error::Mp4(format!("unreasonable chunk count {count}")));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        if wide {
            out.push(read_u64(body, 8 + i * 8)?);
        } else {
            out.push(read_u32(body, 8 + i * 4)? as u64);
        }
    }
    Ok(out)
}

fn parse_stss(body: &[u8]) -> Result<Vec<u32>> {
    let count = read_u32(body, 4)? as usize;
    let mut out = Vec::with_capacity(count.min(1 << 20));
    for i in 0..count {
        out.push(read_u32(body, 8 + i * 4)?);
    }
    Ok(out)
}

fn build_samples(
    stts: &[(u32, u32)],
    ctts: &[(u32, i64)],
    stsc: &[(u32, u32, u32)],
    fixed_size: u32,
    sizes: &[u32],
    sample_count: usize,
    chunk_offsets: &[u64],
    sync_samples: &[u32],
    stss_present: bool,
) -> Result<Vec<Sample>> {
    let mut dts_of = Vec::with_capacity(sample_count);
    let mut dur_of = Vec::with_capacity(sample_count);
    let mut idx = 0usize;
    for &(count, delta) in stts {
        for _ in 0..count {
            if idx >= sample_count {
                break;
            }
            dts_of.push((idx, delta));
            dur_of.push(delta);
            idx += 1;
        }
    }
    while dts_of.len() < sample_count {
        let last_dur = dur_of.last().copied().unwrap_or(0);
        dts_of.push((idx, last_dur));
        dur_of.push(last_dur);
        idx += 1;
    }

    let mut pts_offsets = vec![0i64; sample_count];
    if !ctts.is_empty() {
        let mut i = 0usize;
        for &(count, offset) in ctts {
            for _ in 0..count {
                if i >= sample_count {
                    break;
                }
                pts_offsets[i] = offset;
                i += 1;
            }
        }
    }

    let mut spc_per_chunk = vec![0u32; chunk_offsets.len()];
    for (i, entry) in stsc.iter().enumerate() {
        let first = (entry.0.max(1) as usize).min(chunk_offsets.len() + 1);
        let next_first = stsc
            .get(i + 1)
            .map(|e| e.0.max(1) as usize)
            .unwrap_or(chunk_offsets.len() + 1);
        for chunk in first..next_first.min(chunk_offsets.len() + 1) {
            spc_per_chunk[chunk - 1] = entry.1;
        }
    }

    let sync_set: std::collections::HashSet<u32> = if stss_present && !sync_samples.is_empty() {
        sync_samples.iter().copied().collect()
    } else {
        Default::default()
    };

    let mut samples = Vec::with_capacity(sample_count);
    let mut sample_idx = 0usize;
    let mut dts_acc = 0u64;
    for (chunk_index, &chunk_offset) in chunk_offsets.iter().enumerate() {
        let spc = spc_per_chunk[chunk_index];
        if spc == 0 {
            continue;
        }
        let mut offset = chunk_offset;
        for _ in 0..spc {
            if sample_idx >= sample_count {
                break;
            }
            let size = if fixed_size != 0 {
                fixed_size
            } else {
                sizes[sample_idx]
            };
            let dur = dur_of.get(sample_idx).copied().unwrap_or(0);
            let sample_number = sample_idx as u32 + 1;
            let sync = !stss_present || sync_set.is_empty() || sync_set.contains(&sample_number);
            let pts = if pts_offsets[sample_idx] >= 0 {
                dts_acc + pts_offsets[sample_idx] as u64
            } else {
                dts_acc.saturating_sub((-pts_offsets[sample_idx]) as u64)
            };
            samples.push(Sample {
                offset,
                size,
                dts: dts_acc,
                dur,
                pts,
                sync,
            });
            offset += size as u64;
            dts_acc += dur as u64;
            sample_idx += 1;
        }
        if sample_idx >= sample_count {
            break;
        }
    }
    if sample_idx < sample_count {
        return Err(Error::Mp4(format!(
            "sample tables inconsistent: built {sample_idx} of {sample_count}"
        )));
    }
    Ok(samples)
}

fn parse_trak(trak_body: &[u8]) -> Result<Track> {
    let children = sub_boxes(trak_body)?;
    let tkhd = children
        .iter()
        .find(|(b, _)| &b.kind == b"tkhd")
        .ok_or_else(|| Error::Mp4("trak without tkhd".into()))?;
    let id = parse_tkhd(tkhd.1)?;

    let mdia = children
        .iter()
        .find(|(b, _)| &b.kind == b"mdia")
        .ok_or_else(|| Error::Mp4("trak without mdia".into()))?;
    let mdia_children = sub_boxes(mdia.1)?;
    let mdhd = mdia_children
        .iter()
        .find(|(b, _)| &b.kind == b"mdhd")
        .ok_or_else(|| Error::Mp4("mdia without mdhd".into()))?;
    let (timescale, duration) = parse_mdhd(mdhd.1)?;
    let hdlr = mdia_children
        .iter()
        .find(|(b, _)| &b.kind == b"hdlr")
        .ok_or_else(|| Error::Mp4("mdia without hdlr".into()))?;
    let handler = parse_hdlr(hdlr.1)?;

    let minf = mdia_children
        .iter()
        .find(|(b, _)| &b.kind == b"minf")
        .ok_or_else(|| Error::Mp4("mdia without minf".into()))?;
    let minf_children = sub_boxes(minf.1)?;
    let stbl = minf_children
        .iter()
        .find(|(b, _)| &b.kind == b"stbl")
        .ok_or_else(|| Error::Mp4("minf without stbl".into()))?;
    let stbl_children = sub_boxes(stbl.1)?;

    let find = |kind: &FourCC| {
        stbl_children
            .iter()
            .find(|(b, _)| &b.kind == kind)
            .map(|(b, p)| (b, *p))
    };

    let codec = find(b"stsd")
        .and_then(|(_, p)| parse_stsd_codec(p).ok())
        .unwrap_or(*b"????");
    let stts = find(b"stts")
        .map(|(_, p)| parse_stts(p))
        .transpose()?
        .unwrap_or_default();
    let ctts = find(b"ctts")
        .map(|(_, p)| parse_ctts(p))
        .transpose()?
        .unwrap_or_default();
    let stsc = find(b"stsc")
        .map(|(_, p)| parse_stsc(p))
        .transpose()?
        .unwrap_or_default();
    let (fixed_size, sample_count, sizes) = find(b"stsz")
        .map(|(_, p)| parse_stsz(p))
        .transpose()?
        .unwrap_or((0, 0, Vec::new()));
    let chunk_offsets = if let Some((_, p)) = find(b"co64") {
        parse_stco(p, true)?
    } else if let Some((_, p)) = find(b"stco") {
        parse_stco(p, false)?
    } else {
        Vec::new()
    };
    let stss_present = find(b"stss").is_some();
    let sync_samples = find(b"stss")
        .map(|(_, p)| parse_stss(p))
        .transpose()?
        .unwrap_or_default();

    let samples = build_samples(
        &stts,
        &ctts,
        &stsc,
        fixed_size,
        &sizes,
        sample_count,
        &chunk_offsets,
        &sync_samples,
        stss_present,
    )?;

    Ok(Track {
        id,
        handler,
        timescale,
        duration,
        codec,
        samples,
    })
}

impl Mp4Index {
    pub fn parse(moov_buf: &[u8], moov_start: u64, file_size: u64) -> Result<Self> {
        let (moov_header, _) = parse_box_header(moov_buf, 0, None)?
            .ok_or_else(|| Error::Mp4("empty moov buffer".into()))?;
        if &moov_header.kind != b"moov" {
            return Err(Error::Mp4("buffer does not start with moov".into()));
        }
        let moov_body = payload(moov_buf, &moov_header)?;
        let children = sub_boxes(moov_body)?;

        let mvhd = children
            .iter()
            .find(|(b, _)| &b.kind == b"mvhd")
            .ok_or_else(|| Error::Mp4("moov without mvhd".into()))?;
        let (timescale, duration) = parse_mvhd(mvhd.1)?;
        let fragmented = children.iter().any(|(b, _)| &b.kind == b"mvex");

        let mut tracks = Vec::new();
        for (child, body) in &children {
            if &child.kind == b"trak" {
                tracks.push(parse_trak(body)?);
            }
        }
        if tracks.is_empty() {
            return Err(Error::Mp4("moov without tracks".into()));
        }

        Ok(Mp4Index {
            timescale,
            duration,
            tracks,
            fragmented,
            moov_start,
            moov_end: moov_start + moov_header.size,
            file_size,
            header_ranges: Vec::new(),
        })
    }
}

pub fn is_faststart_moov(moov_start: u64) -> bool {
    moov_start < 1024 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_header_sizes() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"ftyp");
        buf.extend_from_slice(b"isom0000");
        let (header, next) = parse_box_header(&buf, 0, None).unwrap().unwrap();
        assert_eq!(&header.kind, b"ftyp");
        assert_eq!(header.size, 16);
        assert_eq!(next, 16);
    }

    #[test]
    fn box_header_large_size() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&1024u64.to_be_bytes());
        let (header, _) = parse_box_header(&buf, 0, None).unwrap().unwrap();
        assert_eq!(&header.kind, b"mdat");
        assert_eq!(header.size, 1024);
        assert_eq!(header.header_len, 16);
    }

    #[test]
    fn scans_multiple_boxes() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&8u32.to_be_bytes());
        buf.extend_from_slice(b"free");
        buf.extend_from_slice(&8u32.to_be_bytes());
        buf.extend_from_slice(b"skip");
        let boxes = scan_boxes(&buf, None).unwrap();
        assert_eq!(boxes.len(), 2);
        assert_eq!(&boxes[1].kind, b"skip");
    }
}
