slint::include_modules!();

mod player;
mod worker;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use gkuview_core::{human_duration, human_size, log_line, Clip, ClipKind, Config, DashcamClient};
use player::{Player, PlayerState};
use worker::{Command, Event};

struct AppState {
    ui: slint::Weak<MainWindow>,
    tx: Sender<Command>,
    rx: Receiver<Event>,
    config: Config,
    all_clips: RefCell<Vec<Clip>>,
    visible: RefCell<Vec<Clip>>,
    selected: RefCell<Option<Clip>>,
    thumbnails: RefCell<HashMap<String, slint::Image>>,
    client: RefCell<Option<(String, Arc<DashcamClient>)>>,
    player: RefCell<Player>,
    preview_dir: PathBuf,
}

impl AppState {
    fn ui(&self) -> MainWindow {
        self.ui.upgrade().expect("UI was dropped")
    }

    fn host(&self) -> String {
        self.ui().get_host().to_string()
    }

    fn client(&self) -> Arc<DashcamClient> {
        let host = self.host();
        let mut slot = self.client.borrow_mut();
        if let Some((cached_host, client)) = slot.as_ref() {
            if *cached_host == host {
                return client.clone();
            }
        }
        let client = Arc::new(DashcamClient::new(&host, self.config.endpoints.clone()));
        *slot = Some((host, client.clone()));
        client
    }

    fn selected_clip(&self) -> Option<Clip> {
        self.selected.borrow().clone()
    }

    fn preview_path(&self, remote: &str) -> PathBuf {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        remote.hash(&mut hasher);
        self.preview_dir.join(format!(
            "{:016x}-{}.jpg",
            hasher.finish(),
            std::process::id()
        ))
    }

    fn output_path(&self, clip: &Clip, suffix: &str) -> PathBuf {
        if suffix.is_empty() {
            return self.config.output_dir().join(&clip.name);
        }
        let stem = clip
            .name
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(&clip.name);
        self.config.output_dir().join(format!("{stem}{suffix}"))
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    let config = Config::load(None);
    ui.set_host(config.host.clone().into());
    log_line(&format!("gkuview {} starting", gkuview_core::VERSION));

    let (cmd_tx, cmd_rx) = channel();
    let (evt_tx, evt_rx) = channel();
    worker::spawn(cmd_rx, evt_tx, config.clone());

    let preview_dir = std::env::temp_dir().join("gkuview").join("previews");
    let _ = std::fs::create_dir_all(&preview_dir);

    let state = Rc::new(AppState {
        ui: ui.as_weak(),
        tx: cmd_tx,
        rx: evt_rx,
        config,
        all_clips: RefCell::new(Vec::new()),
        visible: RefCell::new(Vec::new()),
        selected: RefCell::new(None),
        thumbnails: RefCell::new(HashMap::new()),
        client: RefCell::new(None),
        player: RefCell::new(Player::new()),
        preview_dir,
    });

    wire_callbacks(&ui, &state);

    let timer = slint::Timer::default();
    let pump_state = state.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(60),
        move || pump(&pump_state),
    );

    ui.run()
}

fn refresh_time_texts(ui: &MainWindow) {
    let start = ui.get_in_time() as f64;
    let end = ui.get_out_time() as f64;
    ui.set_in_time_text(human_duration(start).into());
    ui.set_out_time_text(human_duration(end).into());
    ui.set_selection_text(
        format!(
            "Selection: {} \u{2192} {}  ({})",
            human_duration(start),
            human_duration(end),
            human_duration((end - start).max(0.0))
        )
        .into(),
    );
}

fn set_frame(ui: &MainWindow, image: slint::Image) {
    ui.set_player_frame(image);
    ui.set_has_frame(true);
}

fn rebuild_clips(state: &Rc<AppState>) {
    let ui = state.ui();
    let filter = ui.get_filter_text().to_string().to_lowercase();
    let kind = ui.get_kind_filter();
    let thumbs = state.thumbnails.borrow();
    let selected_path = state.selected.borrow().as_ref().map(|c| c.path.clone());
    let mut visible = Vec::new();
    let mut items = Vec::new();
    let mut selected_index = -1i32;

    for clip in state.all_clips.borrow().iter() {
        if !filter.is_empty() && !clip.name.to_lowercase().contains(&filter) {
            continue;
        }
        match kind {
            1 if clip.kind != ClipKind::Video => continue,
            2 if clip.kind != ClipKind::Photo => continue,
            _ => {}
        }
        if selected_path.as_deref() == Some(clip.path.as_str()) {
            selected_index = items.len() as i32;
        }
        let thumb = thumbs.get(&clip.path).cloned().unwrap_or_default();
        items.push(ClipItem {
            name: clip.name.clone().into(),
            path: clip.path.clone().into(),
            size: clip.size.map(human_size).unwrap_or_default().into(),
            date: clip.recorded_at.clone().unwrap_or_default().into(),
            thumb,
        });
        visible.push(clip.clone());
    }

    ui.set_clips(items.as_slice().into());
    ui.set_selected_index(selected_index);
    *state.visible.borrow_mut() = visible;
}

fn select_clip(state: &Rc<AppState>, clip: &Clip) {
    let ui = state.ui();
    *state.selected.borrow_mut() = Some(clip.clone());
    ui.set_player_title(clip.name.clone().into());
    ui.set_player_error(false);
    ui.set_player_duration(0.0);
    ui.set_player_duration_text("--:--".into());
    ui.set_player_position(0.0);
    ui.set_player_time_text("00:00".into());
    ui.set_clip_duration(0.0);
    ui.set_in_time(0.0);
    ui.set_out_time(1.0);
    ui.set_export_path("".into());
    ui.set_stats_line("".into());

    let poster = state.thumbnails.borrow().get(&clip.path).cloned();
    match poster {
        Some(image) => set_frame(&ui, image),
        None => {
            ui.set_player_frame(slint::Image::default());
            ui.set_has_frame(false);
        }
    }
    ui.set_selected_index(
        state
            .visible
            .borrow()
            .iter()
            .position(|c| c.path == clip.path)
            .map(|i| i as i32)
            .unwrap_or(-1),
    );
    refresh_time_texts(&ui);

    let _ = state.tx.send(Command::FetchDuration {
        host: state.host(),
        clip: clip.clone(),
    });
    state
        .player
        .borrow_mut()
        .play(state.client(), clip.clone(), 0.0);
    ui.set_status(format!("Playing {}", clip.name).into());
}

fn wire_callbacks(ui: &MainWindow, state: &Rc<AppState>) {
    {
        let state = state.clone();
        ui.on_connect_clicked(move || {
            let _ = state.tx.send(Command::Connect { host: state.host() });
        });
    }
    {
        let state = state.clone();
        ui.on_refresh_clicked(move || {
            let _ = state.tx.send(Command::Refresh { host: state.host() });
        });
    }
    {
        let state = state.clone();
        ui.on_filter_changed(move |text| {
            state.ui().set_filter_text(text);
            rebuild_clips(&state);
        });
    }
    {
        let state = state.clone();
        ui.on_kind_filter_changed(move |kind| {
            state.ui().set_kind_filter(kind);
            rebuild_clips(&state);
        });
    }
    {
        let state = state.clone();
        ui.on_clip_activated(move |index| {
            let clip = state.visible.borrow().get(index as usize).cloned();
            if let Some(clip) = clip {
                select_clip(&state, &clip);
            }
        });
    }
    {
        let state = state.clone();
        ui.on_play_pause_clicked(move || {
            let clip = state.selected_clip();
            state.player.borrow_mut().toggle(Some(state.client()), clip);
        });
    }
    {
        let state = state.clone();
        ui.on_position_changed(move |value| {
            let clip = state.selected_clip();
            let value = (value as f64).max(0.0);
            state
                .player
                .borrow_mut()
                .seek(Some(state.client()), clip, value);
            let ui = state.ui();
            ui.set_player_position(value as f32);
            ui.set_player_time_text(human_duration(value).into());
        });
    }
    {
        let state = state.clone();
        ui.on_set_in_clicked(move || {
            let ui = state.ui();
            let position = state.player.borrow().position();
            let duration = ui.get_clip_duration() as f64;
            let value = position.min((duration - 0.1).max(0.0));
            if value >= ui.get_out_time() as f64 {
                ui.set_out_time((value + 1.0).min(duration.max(value + 0.1)) as f32);
            }
            ui.set_in_time(value as f32);
            refresh_time_texts(&ui);
        });
    }
    {
        let state = state.clone();
        ui.on_set_out_clicked(move || {
            let ui = state.ui();
            let position = state.player.borrow().position();
            let duration = ui.get_clip_duration() as f64;
            let value = position.min(duration.max(position));
            if value <= ui.get_in_time() as f64 {
                ui.set_in_time((value - 1.0).max(0.0) as f32);
            }
            ui.set_out_time(value as f32);
            refresh_time_texts(&ui);
        });
    }
    {
        let state = state.clone();
        ui.on_in_changed(move |value| {
            let ui = state.ui();
            let duration = ui.get_clip_duration() as f64;
            let value = (value as f64).max(0.0).min(duration.max(0.1));
            if value >= ui.get_out_time() as f64 - 0.05 {
                let new_out = (value + 1.0).min(duration.max(value + 0.1));
                ui.set_out_time(new_out as f32);
            }
            ui.set_in_time(value as f32);
            refresh_time_texts(&ui);
        });
    }
    {
        let state = state.clone();
        ui.on_out_changed(move |value| {
            let ui = state.ui();
            let duration = ui.get_clip_duration() as f64;
            let value = (value as f64).max(0.0).min(duration.max(0.1));
            if value <= ui.get_in_time() as f64 + 0.05 {
                let new_in = (value - 1.0).max(0.0);
                ui.set_in_time(new_in as f32);
            }
            ui.set_out_time(value as f32);
            refresh_time_texts(&ui);
        });
    }
    {
        let state = state.clone();
        ui.on_preview_clicked(move || {
            let Some(clip) = state.selected_clip() else {
                return;
            };
            let ui = state.ui();
            let at = ui.get_in_time() as f64;
            let out = state.preview_path(&clip.path);
            let _ = state.tx.send(Command::Preview {
                host: state.host(),
                clip,
                at_s: at,
                out,
            });
        });
    }
    {
        let state = state.clone();
        ui.on_export_clicked(move || {
            let Some(clip) = state.selected_clip() else {
                return;
            };
            let ui = state.ui();
            let start = ui.get_in_time() as f64;
            let end = ui.get_out_time() as f64;
            if end <= start {
                ui.set_status("Selection is empty".into());
                return;
            }
            let out = state.output_path(&clip, "_trim.mp4");
            ui.set_export_path(out.display().to_string().into());
            ui.set_stats_line("Downloading only the selected window...".into());
            let _ = state.tx.send(Command::Export {
                host: state.host(),
                clip,
                start_s: start,
                end_s: end,
                out,
            });
        });
    }
    {
        let state = state.clone();
        ui.on_download_clicked(move || {
            let Some(clip) = state.selected_clip() else {
                return;
            };
            let ui = state.ui();
            let out = state.output_path(&clip, "");
            ui.set_export_path(out.display().to_string().into());
            ui.set_stats_line("Downloading full clip...".into());
            let _ = state.tx.send(Command::Download {
                host: state.host(),
                clip,
                out,
            });
        });
    }
    {
        let state = state.clone();
        ui.on_live_clicked(move || {
            let ui = state.ui();
            let url = ui.get_live_url().to_string();
            if url.is_empty() {
                return;
            }
            if !open_external(&url) {
                ui.set_status("Could not launch an external player for the live stream".into());
            }
        });
    }
    {
        let state = state.clone();
        ui.on_open_external_clicked(move || {
            let Some(clip) = state.selected_clip() else {
                return;
            };
            let url = state.client().file_url(&clip.path);
            if !open_external(&url) {
                state
                    .ui()
                    .set_status("Could not launch an external player".into());
            }
        });
    }
    {
        let state = state.clone();
        ui.on_open_folder_clicked(move || {
            let dir = state.config.output_dir();
            let _ = std::fs::create_dir_all(&dir);
            if !open_external_path(&dir) {
                state
                    .ui()
                    .set_status(format!("Output folder: {}", dir.display()).into());
            }
        });
    }
}

fn pump(state: &Rc<AppState>) {
    loop {
        match state.rx.try_recv() {
            Ok(event) => handle_event(state, event),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }

    let snapshot = state.player.borrow_mut().poll();
    let ui = state.ui();
    ui.set_playing(snapshot.state == PlayerState::Playing);
    ui.set_player_position(snapshot.position as f32);
    if let Some(frame) = snapshot.frame {
        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            &frame.data,
            player::FRAME_W,
            player::FRAME_H,
        );
        set_frame(&ui, slint::Image::from_rgba8(buffer));
    }
    ui.set_player_time_text(human_duration(snapshot.position).into());
    if let Some(error) = snapshot.error {
        ui.set_player_error(true);
        ui.set_status(error.clone().into());
        log_line(&format!("player error: {error}"));
    }
}

fn handle_event(state: &Rc<AppState>, event: Event) {
    let ui = state.ui();
    match event {
        Event::Status(message) => ui.set_status(message.into()),
        Event::Busy(busy) => {
            ui.set_busy(busy);
            if !busy {
                ui.set_progress(0.0);
            }
        }
        Event::Progress(fraction) => ui.set_progress(fraction as f32),
        Event::DeviceInfo {
            model,
            firmware,
            rtsp,
        } => {
            ui.set_device_model(
                model
                    .map(|m| format!("Model: {m}"))
                    .unwrap_or_default()
                    .into(),
            );
            ui.set_device_firmware(
                firmware
                    .map(|f| format!("Firmware: {f}"))
                    .unwrap_or_default()
                    .into(),
            );
            ui.set_live_url(rtsp.into());
        }
        Event::Clips(clips) => {
            *state.all_clips.borrow_mut() = clips;
            rebuild_clips(state);
        }
        Event::ThumbnailReady { clip_path, path } => {
            let Ok(image) = slint::Image::load_from_path(&path) else {
                return;
            };
            state
                .thumbnails
                .borrow_mut()
                .insert(clip_path.clone(), image.clone());
            rebuild_clips(state);
            let is_selected = state
                .selected
                .borrow()
                .as_ref()
                .map(|c| c.path == clip_path)
                .unwrap_or(false);
            let player_busy = matches!(
                state.player.borrow().state(),
                PlayerState::Playing | PlayerState::Loading
            );
            if is_selected && !player_busy {
                set_frame(&ui, image);
            }
        }
        Event::Duration {
            clip_path,
            duration_s,
        } => {
            let is_selected = state
                .selected
                .borrow()
                .as_ref()
                .map(|c| c.path == clip_path)
                .unwrap_or(false);
            if is_selected {
                ui.set_clip_duration(duration_s as f32);
                ui.set_out_time(duration_s as f32);
                ui.set_player_duration(duration_s as f32);
                ui.set_player_duration_text(human_duration(duration_s).into());
                refresh_time_texts(&ui);
                state.player.borrow_mut().set_duration(duration_s);
                ui.set_status(format!("{clip_path}: {:.1}s", duration_s).into());
            }
        }
        Event::PreviewReady { clip_path, path } => {
            let is_selected = state
                .selected
                .borrow()
                .as_ref()
                .map(|c| c.path == clip_path)
                .unwrap_or(false);
            if is_selected && !state.player.borrow().is_playing() {
                if let Ok(image) = slint::Image::load_from_path(&path) {
                    set_frame(&ui, image);
                }
            }
        }
        Event::Finished(message) => {
            ui.set_busy(false);
            ui.set_progress(0.0);
            ui.set_stats_line(message.clone().into());
            ui.set_status(message.into());
        }
        Event::Failed(message) => {
            ui.set_busy(false);
            ui.set_progress(0.0);
            ui.set_status(message.into());
        }
    }
}

fn open_external(url: &str) -> bool {
    let attempts: [(&str, Vec<&str>); 3] = [
        ("mpv", vec![url]),
        ("vlc", vec![url]),
        (
            "ffplay",
            vec!["-rtsp_transport", "tcp", "-window_title", "GKUview", url],
        ),
    ];
    for (binary, args) in attempts {
        let result = ProcessCommand::new(binary)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if result.is_ok() {
            return true;
        }
    }
    false
}

fn open_external_path(path: &std::path::Path) -> bool {
    #[cfg(target_os = "linux")]
    let opener = "xdg-open";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(target_os = "windows")]
    let opener = "explorer";
    ProcessCommand::new(opener)
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}
