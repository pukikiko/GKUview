use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

pub fn available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn version() -> Option<String> {
    let output = Command::new("ffmpeg").arg("-version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(|s| s.to_string())
}

fn run_ffmpeg(args: Vec<OsString>) -> Result<()> {
    let output = Command::new("ffmpeg")
        .args(&args)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::FfmpegMissing,
            _ => Error::Io(e),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("unknown ffmpeg failure");
        return Err(Error::Ffmpeg(last.trim().to_string()));
    }
    Ok(())
}

fn os(s: impl Into<OsString>) -> OsString {
    s.into()
}

pub fn trim_copy(input: &Path, start_s: f64, end_s: f64, output: &Path) -> Result<()> {
    let args = vec![
        os("-hide_banner"),
        os("-loglevel"),
        os("error"),
        os("-y"),
        os("-ss"),
        os(format!("{:.3}", start_s.max(0.0))),
        os("-to"),
        os(format!("{:.3}", end_s.max(0.0))),
        os("-i"),
        input.as_os_str().to_os_string(),
        os("-map"),
        os("0"),
        os("-ignore_unknown"),
        os("-c"),
        os("copy"),
        os("-avoid_negative_ts"),
        os("make_zero"),
        os("-movflags"),
        os("+faststart"),
        output.as_os_str().to_os_string(),
    ];
    run_ffmpeg(args)
}

pub fn extract_frame(input: &Path, at_s: f64, output: &Path) -> Result<()> {
    let args = vec![
        os("-hide_banner"),
        os("-loglevel"),
        os("error"),
        os("-y"),
        os("-ss"),
        os(format!("{:.3}", at_s.max(0.0))),
        os("-i"),
        input.as_os_str().to_os_string(),
        os("-frames:v"),
        os("1"),
        os("-update"),
        os("1"),
        output.as_os_str().to_os_string(),
    ];
    run_ffmpeg(args)
}

pub fn probe_duration(input: &Path) -> Result<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(input)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::FfmpegMissing,
            _ => Error::Io(e),
        })?;
    if !output.status.success() {
        return Err(Error::Ffmpeg("ffprobe could not read the file".into()));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .parse::<f64>()
        .map_err(|_| Error::Ffmpeg(format!("could not parse ffprobe duration '{text}'")))
}
