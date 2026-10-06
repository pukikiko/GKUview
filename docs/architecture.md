# Architecture

## Workspace layout

```
GKUview/
├── captures/                 # source material (connection log; raw pcap gitignored)
├── docs/                     # API observations, capture methodology, this file
├── crates/
│   ├── gkuview-core/         # protocol + media engine (no UI)
│   │   ├── src/client.rs     # HTTP client, endpoint probing, listing, thumbnails, RTSP
│   │   ├── src/listing.rs    # tolerant JSON/HTML/text clip-list parsing
│   │   ├── src/mp4.rs        # minimal MP4 (ISO-BMFF) box/sample-table parser
│   │   ├── src/ts.rs         # MPEG-TS PES/PTS parsing and time->byte-range planning
│   │   ├── src/plan.rs       # MP4 time window -> byte-range plan
│   │   ├── src/download.rs   # range downloads (sparse for MP4, contiguous for TS)
│   │   ├── src/crop.rs       # locate index, plan, download, ffmpeg trim
│   │   ├── src/ffmpeg.rs     # ffmpeg/ffprobe process wrappers
│   │   ├── src/endpoints.rs  # pluggable endpoint templates (verified GKU defaults)
│   │   └── src/config.rs     # ~/.config/gkuview/config.json
│   ├── gkuview-cli/          # headless CLI (probe/list/download/duration/preview/crop)
│   └── gkuview/              # Slint desktop app
│       ├── ui/app.slint
│       └── src/{main,worker}.rs
└── Cargo.toml                # workspace
```

`gkuview-core` has no UI dependencies and can be embedded in other tools. `gkuview-cli` uses it
directly; the GUI drives it through a worker thread.

## Threading model (GUI)

```
 main thread (Slint)                    worker thread
 ┌───────────────────────┐   Command   ┌────────────────────────────┐
 │ callbacks ────────────┼────────────>│ DashcamClient              │
 │ Timer (60 ms) <───────┼──── Event ──│ range planning/download    │
 │ pump(): set properties│             │ thumbnails, ffmpeg trim    │
 └───────────────────────┘             └────────────────────────────┘
```

- Only the UI thread touches Slint properties.
- The worker owns an `Arc<DashcamClient>` per host and processes commands sequentially, so a
  single download runs at a time and progress events never race.
- A second background pool (4 threads) fills the clip list with device thumbnails, caching them
  under `~/.cache/gkuview/thumbnails/` and cancelling when a newer list arrives.
- Embedded playback runs in its own threads and never blocks the worker: `player.rs` finds the
  byte offset for the requested time (PES search for TS, start-of-file for MP4), streams
  `Range: bytes=<offset>-` into an ffmpeg process reading MPEG-TS from stdin, decodes to
  480×270 RGBA at 10 fps and publishes the latest frame for the UI timer to display. Seeking
  restarts the pipeline at a new offset; pausing kills it and keeps the position.
- Previews use the camera's `getthumbnail` endpoint when a clip is first selected (fast, no
  ffmpeg), and fall back to local frame extraction for scrubbed timestamps and photos.

## Endpoint resolution

`Endpoints` (in `endpoints.rs`) carries the verified GKU templates (see
[dashcam-api.md §9](dashcam-api.md#9-how-gkuview-maps-to-the-api)):

1. `list_url_template` + `folders` + `page_size` drive paginated listing; pages stop when a page
   is short, empty, or `count <= 0`.
2. If that yields nothing, the client probes `list_candidates` (generic Novatek-style and
   directory-index paths) and caches whichever returns the most clips.
3. `device_info_paths` and `media_info_path` provide model, firmware and per-camera RTSP URLs.
4. `thumbnail_url_template` provides JPEG thumbnails; `file_url_template` builds media URLs.

Everything is overridable in `~/.config/gkuview/config.json`, so new firmwares need config, not
code.

## Partial cropping

The requirement: **exporting a cropped clip must download only the bytes needed for the selected
duration.** Both supported containers are indexed remotely first.

### MP4 (`.mp4`/`.mov`)

1. Probe `HEAD` + `Range: bytes=0-0` → size and range support.
2. Locate `moov`: first 1 MiB, then last 1 MiB, then forward 1 MiB windows; fetch the exact box.
3. Parse `moov` (containers → `mvhd/mdhd/tkhd/hdlr/stsd`, sample tables
   `stts/ctts/stsc/stsz/stco/co64/stss`) into per-track samples. Reject fragmented MP4 (`mvex`).
4. Snap the start to the preceding keyframe, collect sample byte ranges for the window
   (video + audio with 1 s preroll), add `moov`/`ftyp`, pad by 16 bytes, merge gaps under 32 KiB,
   and collapse to the smallest span when that is within 2.5× of the exact byte total.
5. Download ranges into a **sparse file** (holes cost no network), then
   `ffmpeg -ss … -to … -c copy`.

### MPEG-TS (`.ts`, GKU D600Pro)

MPEG-TS has no index, so GKUview finds positions by timestamp and reads the stream at those
positions:

1. `probe`: read the first ~96 KiB and the tail to get the first/last video PES PTS and derive
   duration (`ts.rs`). PTS are 33-bit @ 90 kHz; wraparound is handled by unwrapping near a
   reference value.
2. `find_position`: binary-search the file on byte offset. Each probe fetches a 64 KiB window,
   parses TS packets (188-byte sync lattice, adaptation field, PES headers) and compares the
   first video PTS with the target. ~10 requests for a 47 MB clip, then a final scan picks the
   exact PES packet (`Before` = last PTS ≤ target, `After` = first PTS ≥ target).
3. `find_keyframe`: scan backwards up to 32 MiB in 1 MiB chunks for the last PES packet with the
   adaptation-field *random access indicator* at or before the start position.
4. The plan is **one contiguous range** from that keyframe to the packet after the end position.
   It is downloaded contiguously (a leading hole would break stream detection) and trimmed with
   `ffmpeg -ss (start-keyframe) -to (end-keyframe) -c copy`. ffmpeg interprets input `-ss`
   relative to the file's first timestamp, so non-zero or wrapped PTS start offsets work (covered
   by `tests/ts_plan.rs`).
5. The end position is before the telemetry footer, so exported clips never contain the GPS
   records described in [dashcam-api.md §7](dashcam-api.md#7-gps-telemetry-and-privacy).

### Fallback

If a camera ignores `Range` (no `206`), GKUview downloads the full file, trims locally, and
reports the fallback reason in both the CLI and the GUI.

## Known limitations

| Limitation | Detail / mitigation |
| --- | --- |
| Range support required for partial download | Cameras that ignore `Range` fall back to full download + local trim (correct, slower). |
| Keyframe-aligned starts | Stream-copy starts at the preceding keyframe; the cut point is reported (`keyframe_start_s`). Re-encode would fix it at CPU/quality cost. |
| Fragmented MP4 | `moof`-based recordings fall back to full download for partial requests. |
| `ctts`/B-frames | PTS is reconstructed (with `ctts` when present); exotic edit lists are ignored. |
| TS keyframe detection | Relies on the random access indicator; if absent, the plan degrades to the nearest PES start and ffmpeg may produce a moment of artefacts. |
| Telemetry footer | Ignored entirely; not downloaded during crops. |
| Port 7878 | Advertised, protocol unknown, unused. |
| ffmpeg dependency | Playback, scrubbed previews and exports shell out to `ffmpeg`/`ffprobe`. Listing, thumbnails and index reading work without it. |
| HEVC decoder | GKU media is H.265. Distro ffmpeg builds without an HEVC decoder (e.g. Fedora `ffmpeg-free`) cannot play or frame-grab it; the UI shows a hint and offers external players. |
| Untrusted device input | Remote data is parsed by small hand-written parsers with size/count caps; ffmpeg runs on local files only. |

## Testing

- Unit tests: listing parsing (real GKU JSON sample), endpoint templates, MP4 box parser,
  range merging, TS packet/PES parsing, PTS unwrapping.
- Integration tests (`crates/gkuview-core/tests/`): generated MP4 and TS fixtures verify index
  parsing, byte-range planning, and ffmpeg trimming, including a TS file with a 100 s PTS offset.
- Offline end-to-end: a mock dashcam HTTP server (same JSON shapes, range support) exercises
  `list`, `duration`, `preview` and `crop` through `gkuview-cli` without a camera.

## Security and privacy

- All camera traffic is plain HTTP/RTSP over the camera's own Wi-Fi; there is no TLS and no cloud
  service involved.
- No credentials are stored: `config.json` holds a hostname and URL templates only.
- Captures committed to this repo contain no credentials or identifiers; raw pcaps are
  gitignored because media files embed GPS telemetry.
