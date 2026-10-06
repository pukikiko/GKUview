# GKUview

A desktop **client, clip viewer and clip editor (cropper) for GKU dashcams**, written in
Rust + [Slint](https://slint.dev). Browse recordings over the camera's Wi-Fi, preview frames, and
export a trimmed clip by downloading **only the bytes for the selected duration** using HTTP
range requests.

## Features

- **Player-first UI** — selecting a clip starts embedded playback immediately. The player seeks
  over HTTP ranges (PES-timestamp lookup for `.TS`, `moov` index for MP4) and streams decoded
  frames, with a transport bar, scrubbing, and a filterable clip list with thumbnails.
- **Client** — connect to the dashcam AP (`192.168.0.1`), read model/firmware, browse all
  recording folders (`loop`, `event`, `emr`, …) with pagination, and show device-rendered
  thumbnails in the list.
- **Viewer** — preview frames from the camera (`getthumbnail`) or extracted locally at any
  timestamp; full clip download with progress; live RTSP view in an external player
  (`mpv`/`vlc`/`ffplay`).
- **Clip editor (cropper)** — in/out selection from the playhead or sliders; export downloads only
  the selected window (keyframe-extended), stream-copies with ffmpeg, and reports exactly how
  many bytes were fetched versus the full clip. No re-encoding.
- **Headless CLI** — `gkuview-cli` for scripting: probe endpoints, list, download, measure
  durations, extract preview frames, crop.

## Screens

- **Player** – clip list on the left (thumbnails, filter, type toggle), embedded video with
  transport controls in the centre, trim controls underneath.
- **Live view** – button in the toolbar; launches the camera's RTSP streams in an external
  player.

## Requirements

- Rust stable (edition 2021; MSRV 1.75)
- **ffmpeg/ffprobe** on `PATH` for playback, previews and exports (`sudo apt install ffmpeg`)
  - The GKU D600Pro records **H.265/HEVC**. Some distro builds ship without an HEVC decoder
    (e.g. Fedora's `ffmpeg-free`). Embedded playback then shows a hint; install a full ffmpeg
    (RPM Fusion/`ffmpeg`) or use **Open externally** with VLC/mpv.
- Linux desktop libraries needed by Slint (X11/Wayland, fontconfig)
- The dashcam's Wi-Fi network for real use

## Build & run

```bash
git clone <your-fork-url> GKUview
cd GKUview
cargo build --release

# GUI
./target/release/gkuview

# CLI
./target/release/gkuview-cli --help
```

Tests (no camera needed; fixtures are generated media files):

```bash
cargo test --workspace
```

## CLI usage

```bash
# Endpoints and device identity
gkuview-cli -H 192.168.0.1 probe
gkuview-cli info                 # model, firmware, front/rear RTSP URLs

# Browse
gkuview-cli list
gkuview-cli list --json

# Clip duration (reads the remote index; for TS scans PES timestamps over ranges)
gkuview-cli duration sd//Video/20261006_172024_F.TS

# One preview frame (camera thumbnail when available, else a partial GOP download)
gkuview-cli preview sd//Video/20261006_172024_F.TS --at 00:00:12 -o frame.jpg

# Crop 10s–25s and see how much was actually downloaded
gkuview-cli crop sd//Video/20261006_172024_F.TS \
    --start 00:00:10 --end 00:00:25 -o clip_trim.mp4
```

Example crop output (real numbers from the offline end-to-end test):

```
wrote clips/sample_trim.mp4
output duration:  1.5s
source size:      136.6 KiB
downloaded:       49.4 KiB in 1 range request(s) (63.8% saved)
```

If a camera ignores `Range`, the CLI says so and falls back to downloading the full file before
trimming.

## Configuration

`~/.config/gkuview/config.json` is created on first run. The defaults are the verified GKU
endpoints; everything is overridable:

```json
{
  "host": "192.168.0.1",
  "endpoints": {
    "list_url_template": "/app/getfilelist?folder={folder}&start={start}&end={end}",
    "folders": ["loop", "event", "emr", "park", "race"],
    "page_size": 100,
    "device_info_paths": ["/app/getproductinfo", "/app/getdeviceattr"],
    "media_info_path": "/app/getmediainfo",
    "list_candidates": [
      { "label": "GKU/Shenshu app API, loop folder", "path": "/app/getfilelist?folder=loop&start=0&end=99" }
    ],
    "file_base": "/DCIM/100MEDIA",
    "file_url_template": "{base}/{path}",
    "rtsp_url_template": "rtsp://{host}:554/livestream/12",
    "thumbnail_url_template": "{base}/app/getthumbnail?file={path}",
    "list_size_scale": 1024
  },
  "output_dir": null
}
```

Templates support `{base}`, `{host}`, `{path}`, `{name}`; listing templates additionally support
`{folder}`, `{start}`, `{end}`. Exports default to `~/GKUview/`.

## How partial cropping works

- **MP4**: read the remote `moov` with a few range requests, parse the sample tables, snap to a
  keyframe, download only the selected sample ranges into a sparse file, then
  `ffmpeg -c copy`.
- **MPEG-TS (GKU)**: find the start/end byte offsets by binary-searching PES timestamps over
  range requests, scan back to a random-access point, download one contiguous window (never the
  GPS telemetry footer) and stream-copy it locally.

Full details, fallbacks and limitations are in [docs/architecture.md](docs/architecture.md).

## Documentation

| Document | Contents |
| --- | --- |
| [docs/dashcam-api.md](docs/dashcam-api.md) | Verified GKU D600Pro API: endpoints, listing schema, range support, RTSP, TS/GPS footer notes |
| [docs/architecture.md](docs/architecture.md) | Crates, threading, the MP4/TS range-cropping algorithms, limitations |

## Project status / roadmap

- [x] HTTP client with verified GKU endpoints plus generic fallbacks
- [x] Paginated multi-folder listing, device info, RTSP discovery, thumbnails
- [x] MP4 `moov` locator + sample-table parser
- [x] MPEG-TS PES/PTS parser with binary-search position and keyframe lookup
- [x] Byte-range planner, sparse/contiguous downloaders, ffmpeg stream-copy
- [x] Slint GUI: connect, file list, thumbnails, in/out editor, live view
- [x] Offline tests: generated MP4/TS fixtures and mock-server end-to-end CLI runs
- [ ] Frame-accurate (non-keyframe) export via optional re-encode
- [ ] In-app timeline filmstrip and scrubbing preview
- [ ] Parallel range downloads and resume
- [ ] Embedded live video (no external player)

## License

GPL-3.0-only. See [LICENSE](LICENSE).
