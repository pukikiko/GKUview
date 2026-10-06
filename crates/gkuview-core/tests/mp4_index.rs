use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use gkuview_core::ffmpeg;
use gkuview_core::mp4::{scan_boxes_at, Mp4Index};
use gkuview_core::plan::plan_window;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn load_index(name: &str) -> (Mp4Index, Vec<u8>) {
    let bytes = fs::read(fixture(name)).expect("fixture missing; see tests/fixtures/README.md");
    let boxes = scan_boxes_at(&bytes, 0, Some(bytes.len() as u64)).unwrap();
    let moov = boxes
        .iter()
        .find(|b| &b.kind == b"moov")
        .expect("fixture has no moov");
    let start = moov.start as usize;
    let end = start + moov.size as usize;
    let mut index = Mp4Index::parse(&bytes[start..end], moov.start, bytes.len() as u64).unwrap();
    let mut header_ranges = vec![(0u64, (bytes.len() as u64 - 1).min(15))];
    if let Some(ftyp) = boxes.iter().find(|b| &b.kind == b"ftyp") {
        header_ranges.push((ftyp.start, ftyp.end().saturating_sub(1)));
    }
    index.header_ranges = header_ranges;
    (index, bytes)
}

#[test]
fn parses_faststart_clip() {
    let (index, bytes) = load_index("sample_faststart.mp4");
    assert!(!index.fragmented);
    assert!(
        (index.duration_secs() - 4.0).abs() < 0.2,
        "duration {}",
        index.duration_secs()
    );

    let video = index.video_track().expect("no video track");
    assert_eq!(&video.handler, b"vide");
    assert_eq!(
        video.samples.len(),
        40,
        "expected 40 video samples at 10fps x 4s"
    );
    assert!(
        video.samples.first().unwrap().offset >= 2892,
        "sample offsets must point into mdat"
    );
    assert!(index.audio_track().is_some(), "expected an audio track");

    let sync_count = video.samples.iter().filter(|s| s.sync).count();
    assert_eq!(sync_count, 4, "expected one keyframe per second");

    let first = video.samples.first().unwrap();
    let last = video.samples.last().unwrap();
    assert!(first.dts < last.dts);
    assert!(first.offset + first.size as u64 <= bytes.len() as u64);
    assert!(last.offset + last.size as u64 <= bytes.len() as u64);
}

#[test]
fn parses_moov_at_end_clip() {
    let (index, bytes) = load_index("sample_moov_end.mp4");
    let boxes = scan_boxes_at(&bytes, 0, Some(bytes.len() as u64)).unwrap();
    let moov = boxes.iter().find(|b| &b.kind == b"moov").unwrap();
    assert!(
        moov.start > 100_000,
        "moov should be at the end for this fixture"
    );
    assert!((index.duration_secs() - 4.0).abs() < 0.2);
    assert_eq!(index.video_track().unwrap().samples.len(), 40);
}

#[test]
fn plan_downloads_only_selected_window() {
    let (index, bytes) = load_index("sample_faststart.mp4");
    let plan = plan_window(&index, 1.0, 2.5).unwrap();
    assert!(!plan.ranges.is_empty());
    assert_eq!(plan.source_size, bytes.len() as u64);
    assert!(
        plan.planned_bytes < plan.source_size,
        "planned {} of {} bytes",
        plan.planned_bytes,
        plan.source_size
    );
    assert!((plan.keyframe_start_s - 1.0).abs() < 0.11);
    assert!(plan.ranges.iter().all(|r| r.end < plan.source_size));
}

#[test]
fn planned_window_trims_to_requested_duration() {
    if !ffmpeg::available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let (index, bytes) = load_index("sample_faststart.mp4");
    let plan = plan_window(&index, 1.0, 2.5).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let sparse = dir.path().join("partial.mp4");
    let output = dir.path().join("trimmed.mp4");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&sparse)
            .unwrap();
        file.set_len(plan.ranges.iter().map(|r| r.end).max().unwrap() + 1)
            .unwrap();
        for range in &plan.ranges {
            file.seek(SeekFrom::Start(range.start)).unwrap();
            file.write_all(&bytes[range.start as usize..=range.end as usize])
                .unwrap();
        }
    }

    ffmpeg::trim_copy(&sparse, 1.0, 2.5, &output).expect("ffmpeg trim on sparse file");
    let duration = ffmpeg::probe_duration(&output).expect("probe output");
    assert!(
        (duration - 1.5).abs() < 0.35,
        "expected ~1.5s output, got {duration}s"
    );
}
