use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gkuview_core::ts::{self, Position};
use gkuview_core::{log_line, Clip, DashcamClient};

pub const FRAME_W: u32 = 480;
pub const FRAME_H: u32 = 270;
const FRAME_BYTES: usize = (FRAME_W * FRAME_H * 4) as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Empty,
    Loading,
    Playing,
    Paused,
    Error,
}

pub struct Frame {
    pub data: Vec<u8>,
}

pub struct PlayerSnapshot {
    pub state: PlayerState,
    pub position: f64,
    pub frame: Option<Frame>,
    pub error: Option<String>,
}

pub struct Player {
    state: PlayerState,
    frame: Arc<Mutex<Option<Frame>>>,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
    stderr: Arc<Mutex<Option<String>>>,
    base_s: f64,
    started: Option<Instant>,
    duration: f64,
    pub clip_path: Option<String>,
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

impl Player {
    pub fn new() -> Self {
        Self {
            state: PlayerState::Empty,
            frame: Arc::new(Mutex::new(None)),
            child: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
            stderr: Arc::new(Mutex::new(None)),
            base_s: 0.0,
            started: None,
            duration: 0.0,
            clip_path: None,
        }
    }

    pub fn state(&self) -> PlayerState {
        self.state
    }

    pub fn set_duration(&mut self, duration_s: f64) {
        if duration_s > 0.0 {
            self.duration = duration_s;
        }
    }

    pub fn position(&self) -> f64 {
        let pos = match self.state {
            PlayerState::Playing => {
                self.base_s
                    + self
                        .started
                        .map(|s| s.elapsed().as_secs_f64())
                        .unwrap_or(0.0)
            }
            _ => self.base_s,
        };
        if self.duration > 0.0 {
            pos.min(self.duration)
        } else {
            pos
        }
    }

    pub fn is_playing(&self) -> bool {
        self.state == PlayerState::Playing
    }

    pub fn play(&mut self, client: Arc<DashcamClient>, clip: Clip, at_s: f64) {
        self.stop_pipeline();
        self.duration = 0.0;
        self.clip_path = Some(clip.path.clone());
        self.base_s = at_s.max(0.0);
        self.started = Some(Instant::now());
        self.state = PlayerState::Loading;

        let frame = Arc::new(Mutex::new(None));
        let child_slot = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let stderr_slot = Arc::new(Mutex::new(None));
        self.frame = frame.clone();
        self.child = child_slot.clone();
        self.stop = stop.clone();
        self.stderr = stderr_slot.clone();

        let is_ts = clip.is_mpeg_ts();
        let at_s = at_s.max(0.0);
        std::thread::Builder::new()
            .name("gkuview-player".to_string())
            .spawn(move || {
                if let Err(e) = run_pipeline(
                    client,
                    clip,
                    at_s,
                    is_ts,
                    frame,
                    child_slot,
                    stop,
                    stderr_slot,
                ) {
                    log_line(&format!("player: {e}"));
                }
            })
            .ok();
    }

    pub fn toggle(&mut self, client: Option<Arc<DashcamClient>>, clip: Option<Clip>) {
        match self.state {
            PlayerState::Playing => self.pause(),
            PlayerState::Paused
            | PlayerState::Error
            | PlayerState::Empty
            | PlayerState::Loading => {
                if let (Some(client), Some(clip)) = (client, clip) {
                    let at = self.position();
                    self.play(client, clip, at);
                }
            }
        }
    }

    pub fn pause(&mut self) {
        if self.state == PlayerState::Playing {
            self.base_s = self.position();
            self.started = None;
            self.state = PlayerState::Paused;
            self.kill_child();
        }
    }

    pub fn seek(&mut self, client: Option<Arc<DashcamClient>>, clip: Option<Clip>, at_s: f64) {
        if self.clip_path.is_none() {
            return;
        }
        let at = at_s.max(0.0);
        if self.state == PlayerState::Playing {
            if let (Some(client), Some(clip)) = (client, clip) {
                self.play(client, clip, at);
            }
        } else {
            self.stop_pipeline();
            self.base_s = at;
            self.state = PlayerState::Paused;
        }
    }

    pub fn stop_pipeline(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.kill_child();
    }

    fn kill_child(&self) {
        if let Ok(mut slot) = self.child.lock() {
            if let Some(mut child) = slot.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    pub fn poll(&mut self) -> PlayerSnapshot {
        let frame = self.frame.lock().ok().and_then(|mut f| f.take());
        if frame.is_some() && self.state == PlayerState::Loading {
            self.state = PlayerState::Playing;
        }

        if self.state == PlayerState::Playing {
            if self.duration > 0.0 && self.position() >= self.duration - 0.05 {
                self.base_s = self.duration;
                self.state = PlayerState::Paused;
            }
        }

        let exited = self.child.lock().ok().and_then(|mut slot| {
            let status = slot
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten());
            match status {
                Some(status) => {
                    let child = slot.take();
                    Some((status.success(), child))
                }
                None => None,
            }
        });
        if let Some((success, child)) = exited {
            drop(child);
            if !success {
                let stderr = self
                    .stderr
                    .lock()
                    .ok()
                    .and_then(|s| s.clone())
                    .unwrap_or_default();
                log_line(&format!(
                    "player: ffmpeg exited unsuccessfully: {}",
                    stderr.lines().last().unwrap_or("").trim()
                ));
            }
            if !success && !self.stop.load(Ordering::SeqCst) {
                let stderr = self
                    .stderr
                    .lock()
                    .ok()
                    .and_then(|s| s.clone())
                    .unwrap_or_default();
                let message = playback_hint(&stderr);
                self.state = PlayerState::Error;
                self.started = None;
                return PlayerSnapshot {
                    state: self.state,
                    position: self.position(),
                    frame,
                    error: Some(message),
                };
            }
            if success {
                self.state = PlayerState::Paused;
                self.base_s = self.duration.max(self.position());
                self.started = None;
            }
        }

        PlayerSnapshot {
            state: self.state,
            position: self.position(),
            frame,
            error: None,
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop_pipeline();
    }
}

fn playback_hint(stderr: &str) -> String {
    let tail = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("playback failed")
        .trim()
        .to_string();
    if tail.contains("no decoder") || tail.contains("Decoder not found") {
        format!(
            "{tail}. This ffmpeg build has no HEVC decoder - install full ffmpeg (e.g. RPM Fusion) or use 'Open externally'."
        )
    } else {
        tail
    }
}

#[allow(clippy::too_many_arguments)]
fn run_pipeline(
    client: Arc<DashcamClient>,
    clip: Clip,
    at_s: f64,
    is_ts: bool,
    frame: Arc<Mutex<Option<Frame>>>,
    child_slot: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
    stderr_slot: Arc<Mutex<Option<String>>>,
) -> gkuview_core::Result<()> {
    let url = client.file_url(&clip.path);
    let head = client.head(&url)?;
    let size = head
        .total_size()
        .ok_or_else(|| gkuview_core::Error::Protocol("no content length".into()))?;

    let offset = if is_ts {
        let mut fetch = {
            let client = client.clone();
            let url = url.clone();
            move |start: u64, end: u64| -> gkuview_core::Result<Vec<u8>> {
                Ok(client.get_range(&url, start, end)?.0)
            }
        };
        let info = ts::probe(&mut fetch, size)?;
        let (entry, _) = ts::find_position(&mut fetch, &info, at_s, Position::Before)?;
        let (keyframe, _, _) = ts::find_keyframe(&mut fetch, &info, entry.offset)?;
        keyframe.offset
    } else {
        0
    };
    if stop.load(Ordering::SeqCst) {
        return Ok(());
    }

    let input_format = if is_ts { "mpegts" } else { "mp4" };
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-re",
            "-f",
            input_format,
            "-i",
            "pipe:0",
            "-an",
            "-vf",
            "scale=480:270:force_original_aspect_ratio=decrease,pad=480:270:(ow-iw)/2:(oh-ih)/2",
            "-r",
            "10",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => gkuview_core::Error::FfmpegMissing,
            _ => gkuview_core::Error::Io(e),
        })?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    if let Ok(mut slot) = child_slot.lock() {
        *slot = Some(child);
    }
    log_line(&format!(
        "player: streaming {} from offset {offset} (size {size})",
        clip.path
    ));

    {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("gkuview-player-feed".to_string())
            .spawn(move || {
                let mut progress = |_: u64, _: u64| {};
                let result = client.fetch_range_to(
                    &url,
                    offset,
                    size.saturating_sub(1),
                    &mut stdin,
                    &mut progress,
                );
                if let Err(e) = result {
                    if !stop.load(Ordering::SeqCst) {
                        log_line(&format!("player: feed stopped: {e}"));
                    }
                }
                let _ = stdin.flush();
            })
            .ok();
    }
    {
        let stderr_slot = stderr_slot.clone();
        std::thread::Builder::new()
            .name("gkuview-player-stderr".to_string())
            .spawn(move || {
                let mut reader = stderr;
                let mut text = String::new();
                let _ = reader.read_to_string(&mut text);
                if let Ok(mut slot) = stderr_slot.lock() {
                    *slot = Some(text);
                }
            })
            .ok();
    }

    let mut reader = stdout;
    let mut buffer = vec![0u8; FRAME_BYTES];
    let mut frames = 0usize;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match reader.read_exact(&mut buffer) {
            Ok(()) => {
                frames += 1;
                if let Ok(mut slot) = frame.lock() {
                    *slot = Some(Frame {
                        data: buffer.clone(),
                    });
                }
            }
            Err(_) => break,
        }
    }
    log_line(&format!(
        "player: pipeline ended after {frames} frame(s) (offset {offset})"
    ));
    Ok(())
}
