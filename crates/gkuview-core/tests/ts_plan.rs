use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use gkuview_core::ffmpeg;
use gkuview_core::ts::{self, Position};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn load(name: &str) -> Vec<u8> {
    fs::read(fixture(name)).expect("fixture missing; regenerate per tests/fixtures/README.md")
}

fn fetch_for(bytes: Vec<u8>) -> impl FnMut(u64, u64) -> gkuview_core::Result<Vec<u8>> {
    move |start, end| {
        let len = bytes.len() as u64;
        if end >= len {
            return Err(gkuview_core::Error::InvalidInput(format!(
                "range {start}-{end} beyond fixture ({len} bytes)"
            )));
        }
        Ok(bytes[start as usize..=end as usize].to_vec())
    }
}

#[test]
fn probes_generated_ts() {
    let bytes = load("sample.ts");
    let size = bytes.len() as u64;
    let mut fetch = fetch_for(bytes);
    let info = ts::probe(&mut fetch, size).unwrap();
    assert!(
        (info.duration_s - 4.0).abs() < 0.3,
        "duration {}",
        info.duration_s
    );
    assert!(
        info.first_offset < 188 * 10,
        "first video PES at {} (muxer writes PAT/PMT first)",
        info.first_offset
    );
}

#[test]
fn plan_fetches_only_the_window() {
    let bytes = load("sample.ts");
    let size = bytes.len() as u64;
    let mut fetch = fetch_for(bytes);
    let plan = ts::plan(&mut fetch, size, 1.0, 2.5).unwrap();
    assert!(plan.keyframe_found);
    assert!(
        (plan.keyframe_start_s - 1.0).abs() < 0.15,
        "keyframe at {}",
        plan.keyframe_start_s
    );
    assert!(plan.keyframe_aligned);
    assert!(
        plan.planned_bytes() < size,
        "planned {} of {}",
        plan.planned_bytes(),
        size
    );
    assert!(plan.range_end < size);
}

#[test]
fn position_search_is_monotonic() {
    let bytes = load("sample.ts");
    let size = bytes.len() as u64;
    let mut fetch = fetch_for(bytes);
    let info = ts::probe(&mut fetch, size).unwrap();
    let (before, _) = ts::find_position(&mut fetch, &info, 2.0, Position::Before).unwrap();
    let (after, _) = ts::find_position(&mut fetch, &info, 2.0, Position::After).unwrap();
    assert!(before.offset <= after.offset);
    let rel_before = ts::unwrap_near(before.pts, info.first_pts as i64) - info.first_pts as i64;
    assert!(rel_before as f64 <= 2.0 * ts::PTS_HZ);
}

/// Optional check against a real dashcam recording:
/// `GKUVIEW_REAL_TS=/path/clip.TS cargo test -p gkuview-core --test ts_plan \
///      real_dashcam_clip -- --ignored --nocapture`
#[test]
#[ignore = "set GKUVIEW_REAL_TS to a real dashcam .TS recording"]
fn real_dashcam_clip() {
    let Some(path) = std::env::var_os("GKUVIEW_REAL_TS") else {
        eprintln!("GKUVIEW_REAL_TS not set; skipping");
        return;
    };
    let bytes = fs::read(&path).expect("could not read GKUVIEW_REAL_TS");
    let size = bytes.len() as u64;
    let mut fetch = fetch_for(bytes.clone());
    let info = ts::probe(&mut fetch, size).unwrap();
    eprintln!(
        "probe: size={} first_pts={} last_pts={} duration={:.2}s",
        info.size, info.first_pts, info.last_pts, info.duration_s
    );
    let plan = ts::plan(&mut fetch, size, 5.0, 8.0).unwrap();
    eprintln!(
        "plan: range {}..{} ({} of {} bytes) keyframe={:.2}s found={} aligned={} requests={}",
        plan.range_start,
        plan.range_end,
        plan.planned_bytes(),
        size,
        plan.keyframe_start_s,
        plan.keyframe_found,
        plan.keyframe_aligned,
        plan.requests
    );
    assert!(plan.keyframe_start_s <= 5.2);
    assert!(plan.range_start <= size);
    assert!(plan.planned_bytes() < size);
    assert!(plan.duration_s > 10.0);
}

#[test]
fn trims_planned_range_with_ffmpeg() {
    if !ffmpeg::available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    for name in ["sample.ts", "sample_pts_offset.ts"] {
        let bytes = load(name);
        let size = bytes.len() as u64;
        let mut fetch = fetch_for(bytes.clone());
        let plan = ts::plan(&mut fetch, size, 1.0, 2.5).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("partial.ts");
        let output = dir.path().join("trimmed.mp4");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&partial)
            .unwrap();
        file.write_all(&bytes[plan.range_start as usize..=plan.range_end as usize])
            .unwrap();
        drop(file);

        let rel_start = (plan.start_s - plan.keyframe_start_s).max(0.0);
        let rel_end = (plan.end_s - plan.keyframe_start_s).max(rel_start + 0.05);
        ffmpeg::trim_copy(&partial, rel_start, rel_end, &output)
            .unwrap_or_else(|e| panic!("{name}: trim failed: {e}"));
        let duration = ffmpeg::probe_duration(&output).unwrap();
        assert!(
            (duration - 1.5).abs() < 0.4,
            "{name}: expected ~1.5s, got {duration}s"
        );
    }
}
