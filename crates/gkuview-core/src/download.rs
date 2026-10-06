use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom};
use std::path::Path;

use crate::client::DashcamClient;
use crate::error::Result;
use crate::plan::{ByteRange, Plan};

#[derive(Debug, Clone, Copy, Default)]
pub struct DownloadStats {
    pub bytes_downloaded: u64,
    pub requests: u64,
}

pub type ProgressFn<'a> = &'a mut dyn FnMut(u64, u64);

pub fn download_ranges_to(
    client: &DashcamClient,
    url: &str,
    ranges: &[ByteRange],
    dest: &Path,
    progress: ProgressFn<'_>,
) -> Result<DownloadStats> {
    let total: u64 = ranges.iter().map(|r| r.len()).sum();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(dest)?;
    if let Some(last) = ranges.iter().map(|r| r.end).max() {
        file.set_len(last.saturating_add(1))?;
    }

    let mut stats = DownloadStats::default();
    for range in ranges {
        file.seek(SeekFrom::Start(range.start))?;
        let already = stats.bytes_downloaded;
        client.fetch_range_to(url, range.start, range.end, &mut file, &mut |done, _| {
            progress(already + done, total)
        })?;
        stats.requests += 1;
        stats.bytes_downloaded += range.len();
    }
    progress(stats.bytes_downloaded, total);
    Ok(stats)
}

/// Downloads one range and writes it contiguously from offset 0.
///
/// Used for MPEG-TS windows, where a hole at the start of the file would break
/// stream detection.
pub fn download_range_contiguous(
    client: &DashcamClient,
    url: &str,
    range: ByteRange,
    dest: &Path,
    progress: ProgressFn<'_>,
) -> Result<DownloadStats> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(dest)?;
    client.fetch_range_to(url, range.start, range.end, &mut file, progress)?;
    Ok(DownloadStats {
        bytes_downloaded: range.len(),
        requests: 1,
    })
}

pub fn download_plan_to(
    client: &DashcamClient,
    url: &str,
    plan: &Plan,
    dest: &Path,
    progress: ProgressFn<'_>,
) -> Result<DownloadStats> {
    let stats = download_ranges_to(client, url, &plan.ranges, dest, progress)?;
    Ok(DownloadStats {
        bytes_downloaded: stats.bytes_downloaded,
        requests: stats.requests,
    })
}

pub fn download_full_to(
    client: &DashcamClient,
    url: &str,
    dest: &Path,
    progress: ProgressFn<'_>,
) -> Result<DownloadStats> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(dest)?;
    let bytes = client.download_to(url, &mut file, progress)?;
    Ok(DownloadStats {
        bytes_downloaded: bytes,
        requests: 1,
    })
}
