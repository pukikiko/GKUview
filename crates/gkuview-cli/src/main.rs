use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use gkuview_core::{
    clip_duration, crop_clip, human_duration, human_size, parse_timecode, preview_frame, Clip,
    Config, DashcamClient, Error, Result,
};

const HELP: &str = "\
GKUview CLI - list, download, preview and crop clips from a GKU dashcam

USAGE:
    gkuview-cli [OPTIONS] <COMMAND> [ARGS]

GLOBAL OPTIONS:
    -H, --host <ADDR>      Dashcam address (default 192.168.0.1)
    -c, --config <FILE>    Config file (default ~/.config/gkuview/config.json)
        --json             Print machine-readable JSON where possible
    -h, --help             Print this help

COMMANDS:
    probe                              Probe candidate listing endpoints
    info                               Fetch device info
    list                               List clips on the dashcam
    duration <REMOTE>                  Report clip duration (reads moov via ranges)
    download <REMOTE> [-o FILE]        Download a clip in full
    preview <REMOTE> --at <TIME> [-o FILE]
                                       Download only the needed GOP and extract a frame
    crop <REMOTE> --start <TIME> --end <TIME> [-o FILE]
                                       Download only the selected window and trim it

TIME FORMAT:
    Seconds (12.5) or clock format (00:01:23.5)

REMOTE:
    A path on the device (/DCIM/100MEDIA/FILE.MP4) or a bare file name
";

struct Args {
    host: Option<String>,
    config: Option<PathBuf>,
    json: bool,
    command: String,
    positionals: Vec<String>,
    options: HashMap<String, String>,
}

fn parse_args() -> std::result::Result<Args, String> {
    let mut host = None;
    let mut config = None;
    let mut json = false;
    let mut command = None;
    let mut positionals = Vec::new();
    let mut options = HashMap::new();

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        if command.is_none() && (arg == "-h" || arg == "--help") {
            return Err(String::new());
        }
        match arg.as_str() {
            "-H" | "--host" => host = iter.next(),
            "-c" | "--config" => config = iter.next().map(PathBuf::from),
            "--json" => json = true,
            "-o" | "--output" | "--start" | "--end" | "--at" => {
                let key = match arg.as_str() {
                    "-o" | "--output" => "output",
                    "--start" => "start",
                    "--end" => "end",
                    _ => "at",
                };
                let value = iter.next().ok_or_else(|| format!("{arg} needs a value"))?;
                options.insert(key.to_string(), value);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option {other}"));
            }
            other => {
                if command.is_none() {
                    command = Some(other.to_string());
                } else {
                    positionals.push(other.to_string());
                }
            }
        }
    }

    Ok(Args {
        host,
        config,
        json,
        command: command.unwrap_or_default(),
        positionals,
        options,
    })
}

fn resolve_remote(client: &DashcamClient, remote: &str) -> Clip {
    if remote.starts_with('/') || remote.starts_with("sd/") {
        Clip::new(remote)
    } else {
        let base = client.endpoints().file_base.trim_end_matches('/');
        Clip::new(&format!("{base}/{remote}"))
    }
}

fn output_path(args: &Args, config: &Config, remote: &str, suffix: &str) -> PathBuf {
    if let Some(path) = args.options.get("output") {
        return PathBuf::from(path);
    }
    let name = remote.rsplit('/').next().unwrap_or(remote);
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    config.output_dir().join(format!("{stem}{suffix}"))
}

fn progress_printer() -> impl FnMut(u64, u64) {
    let terminal = std::io::stderr().is_terminal();
    let mut last = Instant::now() - Duration::from_secs(1);
    move |done: u64, total: u64| {
        if last.elapsed() < Duration::from_millis(200) && done < total {
            return;
        }
        last = Instant::now();
        if total > 0 {
            let pct = done as f64 / total as f64 * 100.0;
            if terminal {
                eprint!(
                    "\r  {:.1}%  {} / {}   ",
                    pct,
                    human_size(done),
                    human_size(total)
                );
            } else {
                eprintln!(
                    "  {:.1}%  {} / {}",
                    pct,
                    human_size(done),
                    human_size(total)
                );
            }
        } else if terminal {
            eprint!("\r  {}   ", human_size(done));
        } else {
            eprintln!("  {}", human_size(done));
        }
        if done >= total && terminal {
            eprintln!();
        }
    }
}

fn parse_time(value: &str, what: &str) -> Result<f64> {
    parse_timecode(value).ok_or_else(|| Error::InvalidInput(format!("bad {what} time '{value}'")))
}

fn run() -> Result<i32> {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            if message.is_empty() {
                println!("{HELP}");
                return Ok(0);
            }
            eprintln!("error: {message}\n\n{HELP}");
            return Ok(2);
        }
    };

    if args.command.is_empty() {
        println!("{HELP}");
        return Ok(2);
    }

    let config = Config::load(args.config.as_deref());
    let host = args.host.clone().unwrap_or_else(|| config.host.clone());
    let client = DashcamClient::new(&host, config.endpoints.clone());

    match args.command.as_str() {
        "probe" => {
            for report in client.probe() {
                if args.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "label": report.label,
                            "path": report.path,
                            "url": report.url,
                            "status": report.status,
                            "content_type": report.content_type,
                            "clips": report.clips,
                            "sample": report.sample,
                            "error": report.error,
                        })
                    );
                } else {
                    println!("{}", report.label);
                    println!("  url:    {}", report.url);
                    if let Some(err) = &report.error {
                        println!("  error:  {err}");
                    } else {
                        println!(
                            "  status: {}  type: {}  clips: {}",
                            report.status, report.content_type, report.clips
                        );
                        if !report.sample.is_empty() {
                            let sample = report.sample.replace('\n', " ");
                            println!("  sample: {}", sample.chars().take(160).collect::<String>());
                        }
                    }
                }
            }
            Ok(0)
        }
        "info" => {
            let info = client.device_info()?;
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "host": info.host,
                        "model": info.model,
                        "firmware": info.firmware,
                    })
                );
            } else {
                println!("host:     {}", info.host);
                println!("model:    {}", info.model.as_deref().unwrap_or("unknown"));
                println!(
                    "firmware: {}",
                    info.firmware.as_deref().unwrap_or("unknown")
                );
                match client.rtsp_urls() {
                    Ok(urls) => {
                        for (i, url) in urls.iter().enumerate() {
                            let label = if i == 0 { "front" } else { "rear" };
                            println!("rtsp {label}:  {url}");
                        }
                    }
                    Err(_) => println!("rtsp:     {}", client.rtsp_url()),
                }
            }
            Ok(0)
        }
        "list" => {
            let clips = client.list_clips()?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&clips)?);
            } else {
                println!("{} clip(s) on {}", clips.len(), client.host());
                for clip in &clips {
                    println!(
                        "  {:<36} {:>10}  {}",
                        clip.name,
                        clip.size.map(human_size).unwrap_or_else(|| "-".into()),
                        clip.recorded_at.as_deref().unwrap_or("")
                    );
                }
            }
            Ok(0)
        }
        "duration" => {
            let remote = args.positionals.first().ok_or_else(|| {
                Error::InvalidInput("usage: gkuview-cli duration <REMOTE>".into())
            })?;
            let clip = resolve_remote(&client, remote);
            let duration = clip_duration(&client, &clip)?;
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({"clip": clip.path, "duration_s": duration})
                );
            } else {
                println!("{}: {}", clip.path, human_duration(duration));
            }
            Ok(0)
        }
        "download" => {
            let remote = args.positionals.first().ok_or_else(|| {
                Error::InvalidInput("usage: gkuview-cli download <REMOTE> [-o FILE]".into())
            })?;
            let url = client.file_url(remote);
            let dest = match args.options.get("output") {
                Some(path) => PathBuf::from(path),
                None => {
                    let name = remote.rsplit('/').next().unwrap_or(remote);
                    config.output_dir().join(name)
                }
            };
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut progress = progress_printer();
            let file = std::fs::File::create(&dest)?;
            let mut writer = std::io::BufWriter::new(file);
            let bytes = client.download_to(&url, &mut writer, &mut progress)?;
            drop(writer);
            println!("downloaded {} to {}", human_size(bytes), dest.display());
            Ok(0)
        }
        "preview" => {
            let remote = args.positionals.first().ok_or_else(|| {
                Error::InvalidInput(
                    "usage: gkuview-cli preview <REMOTE> --at <TIME> [-o FILE]".into(),
                )
            })?;
            let at = args
                .options
                .get("at")
                .ok_or_else(|| Error::InvalidInput("--at <TIME> is required".into()))?;
            let at = parse_time(at, "preview")?;
            let clip = resolve_remote(&client, remote);
            let dest = output_path(&args, &config, remote, ".png");
            let mut progress = progress_printer();
            preview_frame(&client, &clip, at, &dest, &mut progress)?;
            println!("wrote preview frame at {:.3}s to {}", at, dest.display());
            Ok(0)
        }
        "crop" => {
            let remote = args.positionals.first().ok_or_else(|| {
                Error::InvalidInput(
                    "usage: gkuview-cli crop <REMOTE> --start <TIME> --end <TIME> [-o FILE]".into(),
                )
            })?;
            let start = args
                .options
                .get("start")
                .ok_or_else(|| Error::InvalidInput("--start <TIME> is required".into()))?;
            let end = args
                .options
                .get("end")
                .ok_or_else(|| Error::InvalidInput("--end <TIME> is required".into()))?;
            let start = parse_time(start, "start")?;
            let end = parse_time(end, "end")?;
            let clip = resolve_remote(&client, remote);
            let dest = output_path(&args, &config, remote, "_trim.mp4");
            let mut progress = progress_printer();
            let report = crop_clip(&client, &clip, start, end, &dest, &mut progress)?;
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "output": report.output,
                        "clip": report.clip_name,
                        "source_size": report.source_size,
                        "downloaded_bytes": report.downloaded_bytes,
                        "planned_bytes": report.planned_bytes,
                        "requests": report.requests,
                        "saved_percent": report.saved_percent(),
                        "start_s": report.start_s,
                        "end_s": report.end_s,
                        "keyframe_start_s": report.keyframe_start_s,
                        "keyframe_aligned": report.keyframe_aligned,
                        "output_duration_s": report.output_duration_s,
                        "used_range_requests": report.used_range_requests,
                        "fallback_reason": report.fallback_reason,
                    })
                );
            } else {
                println!("wrote {}", report.output.display());
                if let Some(duration) = report.output_duration_s {
                    let text = if duration < 60.0 {
                        format!("{duration:.1}s")
                    } else {
                        human_duration(duration)
                    };
                    println!("output duration:  {text}");
                }
                println!("source size:      {}", human_size(report.source_size));
                if report.used_range_requests {
                    println!(
                        "downloaded:       {} in {} range request(s) ({:.1}% saved)",
                        human_size(report.downloaded_bytes),
                        report.requests,
                        report.saved_percent()
                    );
                    if !report.keyframe_aligned
                        && (report.keyframe_start_s - report.start_s).abs() > 0.05
                    {
                        println!(
                            "note:              start snapped to keyframe at {:.3}s",
                            report.keyframe_start_s
                        );
                    }
                } else {
                    println!(
                        "downloaded:       {} (full file; range requests unavailable)",
                        human_size(report.downloaded_bytes)
                    );
                    if let Some(reason) = &report.fallback_reason {
                        println!("fallback:          {reason}");
                    }
                }
            }
            Ok(0)
        }
        other => {
            eprintln!("unknown command '{other}'\n\n{HELP}");
            Ok(2)
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
