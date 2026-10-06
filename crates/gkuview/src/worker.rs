use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gkuview_core::{
    clip_duration, crop_clip, human_size, log_line, preview_frame, Clip, ClipKind, Config,
    DashcamClient,
};

#[derive(Debug, Clone)]
pub enum Command {
    Connect {
        host: String,
    },
    Refresh {
        host: String,
    },
    FetchDuration {
        host: String,
        clip: Clip,
    },
    Preview {
        host: String,
        clip: Clip,
        at_s: f64,
        out: PathBuf,
    },
    Export {
        host: String,
        clip: Clip,
        start_s: f64,
        end_s: f64,
        out: PathBuf,
    },
    Download {
        host: String,
        clip: Clip,
        out: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub enum Event {
    Status(String),
    Busy(bool),
    Progress(f64),
    DeviceInfo {
        model: Option<String>,
        firmware: Option<String>,
        rtsp: String,
    },
    Clips(Vec<Clip>),
    Duration {
        clip_path: String,
        duration_s: f64,
    },
    PreviewReady {
        clip_path: String,
        path: PathBuf,
    },
    ThumbnailReady {
        clip_path: String,
        path: PathBuf,
    },
    Finished(String),
    Failed(String),
}

pub fn spawn(rx: Receiver<Command>, tx: Sender<Event>, config: Config) {
    std::thread::Builder::new()
        .name("gkuview-worker".to_string())
        .spawn(move || run(rx, tx, config))
        .expect("failed to spawn worker thread");
}

fn send(tx: &Sender<Event>, event: Event) {
    if let Event::Failed(message) = &event {
        log_line(&format!("error: {message}"));
    }
    let _ = tx.send(event);
}

fn client_for(
    clients: &mut HashMap<String, Arc<DashcamClient>>,
    host: &str,
    config: &Config,
) -> Arc<DashcamClient> {
    clients
        .entry(host.to_string())
        .or_insert_with(|| Arc::new(DashcamClient::new(host, config.endpoints.clone())))
        .clone()
}

fn progress_channel(tx: Sender<Event>) -> impl FnMut(u64, u64) {
    let mut last = Instant::now() - Duration::from_secs(1);
    move |done: u64, total: u64| {
        if last.elapsed() < Duration::from_millis(100) && done < total {
            return;
        }
        last = Instant::now();
        let fraction = if total > 0 {
            done as f64 / total as f64
        } else {
            0.0
        };
        send(&tx, Event::Progress(fraction.clamp(0.0, 1.0)));
    }
}

fn thumbnail_cache_path(cache_dir: &std::path::Path, host: &str, remote: &str) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    host.hash(&mut hasher);
    remote.hash(&mut hasher);
    cache_dir
        .join("thumbnails")
        .join(format!("{:016x}.jpg", hasher.finish()))
}

/// Fetches list thumbnails in the background, several at a time, reusing the
/// on-disk cache. Stops early when a newer clip list has replaced this one.
fn spawn_thumbnail_fetch(
    client: Arc<DashcamClient>,
    clips: Vec<Clip>,
    generation: u64,
    generation_now: Arc<AtomicU64>,
    cache_dir: PathBuf,
    tx: Sender<Event>,
) {
    let clips = Arc::new(clips);
    let next = Arc::new(AtomicUsize::new(0));
    let worker_count = 4.min(clips.len().max(1));
    for _ in 0..worker_count {
        let client = client.clone();
        let clips = clips.clone();
        let next = next.clone();
        let tx = tx.clone();
        let generation_now = generation_now.clone();
        let cache_dir = cache_dir.clone();
        std::thread::Builder::new()
            .name("gkuview-thumbnails".to_string())
            .spawn(move || loop {
                if generation_now.load(Ordering::SeqCst) != generation {
                    return;
                }
                let index = next.fetch_add(1, Ordering::SeqCst);
                let Some(clip) = clips.get(index) else {
                    return;
                };
                let path = thumbnail_cache_path(&cache_dir, client.host(), &clip.path);
                if !path.exists() {
                    match client.fetch_thumbnail(clip) {
                        Ok(Some(bytes)) => {
                            if let Some(parent) = path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            if std::fs::write(&path, bytes).is_err() {
                                continue;
                            }
                        }
                        _ => continue,
                    }
                }
                if generation_now.load(Ordering::SeqCst) != generation {
                    return;
                }
                send(
                    &tx,
                    Event::ThumbnailReady {
                        clip_path: clip.path.clone(),
                        path,
                    },
                );
            })
            .ok();
    }
}

fn run(rx: Receiver<Command>, tx: Sender<Event>, config: Config) {
    let mut clients: HashMap<String, Arc<DashcamClient>> = HashMap::new();
    let thumbnail_generation = Arc::new(AtomicU64::new(0));
    while let Ok(command) = rx.recv() {
        match command {
            Command::Connect { host } => {
                send(&tx, Event::Busy(true));
                send(&tx, Event::Status(format!("Connecting to {host}...")));
                let client = client_for(&mut clients, &host, &config);
                match client.device_info() {
                    Ok(info) => {
                        let rtsp = client
                            .rtsp_urls()
                            .ok()
                            .and_then(|urls| urls.into_iter().next())
                            .unwrap_or_else(|| client.rtsp_url());
                        send(
                            &tx,
                            Event::DeviceInfo {
                                model: info.model,
                                firmware: info.firmware,
                                rtsp,
                            },
                        );
                        send(
                            &tx,
                            Event::Status("Connected. Loading clips...".to_string()),
                        );
                        match client.list_clips() {
                            Ok(clips) => {
                                send(
                                    &tx,
                                    Event::Status(format!("{} clip(s) on {}", clips.len(), host)),
                                );
                                let generation =
                                    thumbnail_generation.fetch_add(1, Ordering::SeqCst) + 1;
                                send(&tx, Event::Clips(clips.clone()));
                                spawn_thumbnail_fetch(
                                    client.clone(),
                                    clips,
                                    generation,
                                    thumbnail_generation.clone(),
                                    Config::cache_dir(),
                                    tx.clone(),
                                );
                            }
                            Err(e) => send(
                                &tx,
                                Event::Failed(format!("connected, but listing clips failed: {e}")),
                            ),
                        }
                    }
                    Err(e) => send(
                        &tx,
                        Event::Failed(format!("could not reach dashcam at {host}: {e}")),
                    ),
                }
                send(&tx, Event::Busy(false));
            }
            Command::Refresh { host } => {
                send(&tx, Event::Busy(true));
                send(&tx, Event::Status("Loading clips...".to_string()));
                let client = client_for(&mut clients, &host, &config);
                match client.list_clips() {
                    Ok(clips) => {
                        send(
                            &tx,
                            Event::Status(format!("{} clip(s) on {}", clips.len(), host)),
                        );
                        let generation = thumbnail_generation.fetch_add(1, Ordering::SeqCst) + 1;
                        send(&tx, Event::Clips(clips.clone()));
                        spawn_thumbnail_fetch(
                            client.clone(),
                            clips,
                            generation,
                            thumbnail_generation.clone(),
                            Config::cache_dir(),
                            tx.clone(),
                        );
                    }
                    Err(e) => send(&tx, Event::Failed(format!("listing clips failed: {e}"))),
                }
                send(&tx, Event::Busy(false));
            }
            Command::FetchDuration { host, clip } => {
                let client = client_for(&mut clients, &host, &config);
                match clip_duration(&client, &clip) {
                    Ok(duration_s) => send(
                        &tx,
                        Event::Duration {
                            clip_path: clip.path.clone(),
                            duration_s,
                        },
                    ),
                    Err(e) => send(
                        &tx,
                        Event::Failed(format!("could not read clip duration: {e}")),
                    ),
                }
            }
            Command::Preview {
                host,
                clip,
                at_s,
                out,
            } => {
                send(&tx, Event::Busy(true));
                send(
                    &tx,
                    Event::Status(format!("Fetching preview frame at {at_s:.1}s...")),
                );
                let client = client_for(&mut clients, &host, &config);
                let mut progress = progress_channel(tx.clone());
                let result: gkuview_core::Result<()> = (|| {
                    if at_s < 0.6 {
                        if let Ok(Some(bytes)) = client.fetch_thumbnail(&clip) {
                            if let Some(parent) = out.parent() {
                                std::fs::create_dir_all(parent)?;
                            }
                            std::fs::write(&out, bytes)?;
                            return Ok(());
                        }
                    }
                    if clip.kind == ClipKind::Photo {
                        let url = client.file_url(&clip.path);
                        let file = std::fs::File::create(&out)?;
                        let mut writer = std::io::BufWriter::new(file);
                        client.download_to(&url, &mut writer, &mut progress)?;
                        return Ok(());
                    }
                    preview_frame(&client, &clip, at_s, &out, &mut progress)
                })();
                match result {
                    Ok(()) => send(
                        &tx,
                        Event::PreviewReady {
                            clip_path: clip.path.clone(),
                            path: out,
                        },
                    ),
                    Err(e) => send(&tx, Event::Failed(format!("preview failed: {e}"))),
                }
                send(&tx, Event::Busy(false));
            }
            Command::Export {
                host,
                clip,
                start_s,
                end_s,
                out,
            } => {
                send(&tx, Event::Busy(true));
                send(
                    &tx,
                    Event::Status(format!("Downloading {start_s:.1}s - {end_s:.1}s...")),
                );
                let client = client_for(&mut clients, &host, &config);
                let mut progress = progress_channel(tx.clone());
                match crop_clip(&client, &clip, start_s, end_s, &out, &mut progress) {
                    Ok(report) => {
                        let message = if report.used_range_requests {
                            format!(
                                "Exported {} - downloaded {} of {} ({:.1}% saved, {} request(s)){}",
                                report.output.display(),
                                human_size(report.downloaded_bytes),
                                human_size(report.source_size),
                                report.saved_percent(),
                                report.requests,
                                match report.output_duration_s {
                                    Some(d) => format!(", {d:.1}s output"),
                                    None => String::new(),
                                }
                            )
                        } else {
                            format!(
                                "Exported {} (full file downloaded: {})",
                                report.output.display(),
                                report
                                    .fallback_reason
                                    .unwrap_or_else(|| "range requests unavailable".to_string())
                            )
                        };
                        send(&tx, Event::Finished(message));
                    }
                    Err(e) => send(&tx, Event::Failed(format!("export failed: {e}"))),
                }
                send(&tx, Event::Busy(false));
            }
            Command::Download { host, clip, out } => {
                send(&tx, Event::Busy(true));
                send(&tx, Event::Status(format!("Downloading {}...", clip.name)));
                let client = client_for(&mut clients, &host, &config);
                let url = client.file_url(&clip.path);
                if let Some(parent) = out.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let result = (|| -> gkuview_core::Result<u64> {
                    let file = std::fs::File::create(&out)?;
                    let mut writer = std::io::BufWriter::new(file);
                    let mut progress = progress_channel(tx.clone());
                    let bytes = client.download_to(&url, &mut writer, &mut progress)?;
                    Ok(bytes)
                })();
                match result {
                    Ok(bytes) => send(
                        &tx,
                        Event::Finished(format!(
                            "Downloaded {} to {}",
                            human_size(bytes),
                            out.display()
                        )),
                    ),
                    Err(e) => send(&tx, Event::Failed(format!("download failed: {e}"))),
                }
                send(&tx, Event::Busy(false));
            }
        }
    }
}
