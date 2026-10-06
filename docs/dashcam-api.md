# GKU dashcam API — verified on a D600Pro

## 1. API conventions

- Everything (including "commands") is a **GET with query parameters**:
  `GET /app/<command>?param=...`
- Responses are `Content-Type: text/plain; charset=UTF-8`, usually JSON:
  - success: `{"result":0,"info": ... }` (`info` may be object, array, or string)
  - failure: `{"result":-1,"info":"get list failed"}` (status is still `200`)
- A few legacy calls return plain text (`SvrFuncResult="0xFFFFFFFF"`).
- No authentication: the app sends `Cookie: SessionID=null`, and the server ignores it.
- Connections are `Connection: close`; `Last-Modified` is present on media.
- `HEAD` works, and media supports byte ranges:
  `Accept-Ranges: bytes`, `206 Partial Content`,
  `Content-Range: bytes start-end/total`.

## 2. Endpoints

### Read-only (used by GKUview)

| Endpoint | Purpose | Response shape |
| --- | --- | --- |
| `/app/getproductinfo` | Model / vendor | `{"result":0,"info":{"model":"D600Pro","company":"GKUGO","soc":"hisoc","sp":"GKUVISION"}}` |
| `/app/getdeviceattr` | Firmware, camera count, Wi-Fi | `info.softver`, `info.hwver`, `info.camnum`, `info.uuid`, `info.ssid`, `info.wifireboot` |
| `/app/getmediainfo` | Live stream URLs | `info.rtsps: ["rtsp://192.168.0.1/livestream/12","rtsp://192.168.0.1/livestream/13"]`, `info.transport: "tcp"`, `info.port: 7878` |
| `/app/getfilelist?folder=…&start=…&end=…` | Paginated clip list | see §4 |
| `/app/getthumbnail?file=<path>` | JPEG thumbnail (~5–6 KB) | `image/jpeg` |
| `/app/getsdinfo` | Card space (`free`/`total`, MiB) | `{"result":0,"info":{"status":0,"free":19497,"total":122209}}` |
| `/app/getrecduration` | Remaining seconds of current recording segment | `{"result":0,"info":{"duration":27}}` |
| `/app/getparamitems?param=all` / `getparamvalue?param=all\|rec` | Settings catalogue/values | array of `{"name","value"}` |
| `/app/capability`, `/app/getadasitems`, `/app/getadasvalue` | Feature/ADAS detection | JSON |
| `/app/getlockvideostatus`, `/app/getspeech` | Status flags | JSON |
| `/sd/<path>` | Media file download (supports `Range`) | binary |
| `/GPSdata/<file>.TXT` | GPS sidecar — **404 on this firmware** | HTML 404 page |

### Mutating / playback control (observed, not used by GKUview)

`/app/enterrecorder`, `/app/setparamvalue?param=rec&value=0`, `/app/snapshot`,
`/app/setting?param=enter|exit`, `/app/playback?param=enter`,
`/app/setsystime?date=YYYYMMDDhhmmss`, `/app/settimezone?timezone=10_30`.

## 3. File listing

```
GET /app/getfilelist?folder=loop&start=0&end=99
```

Folders observed: `loop` (normal recordings), `emr` (emergency/event videos), `event`
(photos), `park`, `race`. `park` and `race` return `{"result":-1,"info":"get list failed"}` on
this model. Page size is 100.

```json
{"result":0,"info":[{"folder":"loop","count":100,"files":[
  {"name":"sd//Video/20261006_174438_F.TS","duration":-1,"type":2,"size":394730,
   "createtimestr":"20261006174938"},
  {"name":"sd//Photo/20261006_175058_F.JPG","duration":0,"type":1,"size":230,
   "createtimestr":"20261006175058"}
]}]}
```

Field semantics (verified):

| Field | Meaning |
| --- | --- |
| `name` | Device path **including mount**: `sd//Video/…` (front), `sd//Video_rear/…` (rear), `sd//Photo/…`, `sd//Event/…`. Use as-is after `http://192.168.0.1/`. |
| `type` | `1` = photo (JPG), `2` = video (TS) |
| `size` | **KiB** (a 47,054,012-byte clip was listed as `45951`). The device adds ~1 TS packet + footer beyond `size*1024`. |
| `duration` | `-1` = unknown (all videos on this firmware). GKUview measures duration from the TS itself. |
| `createtimestr` | `YYYYMMDDhhmmss`; for split recordings this is the **finalisation** time, while `name` holds the start time (they differ by the segment length). |
| `count` | Files in this page; **negative when `start` is past the end** (e.g. `-5` means 5 beyond). Stop when files are empty or `count <= 0`. |

Listing is newest-first. GKUview walks every folder, paging by 100 until a short/empty page,
then sorts all entries by timestamp.

## 4. Media files

- URL: `http://192.168.0.1/sd//Video/20261006_172024_F.TS` (note the double slash after `sd/`).
- Container: **MPEG-TS**, 188-byte packets, H.265 (HEVC) video from the same encoder as the live
  stream. Front/rear files pair by timestamp with `_F` / `_R` suffixes.
- `thttpd` honours `Range` for arbitrary offsets and does **not** align to packet boundaries;
  players resync on the TS sync byte. The app (`Lavf/57.83.100`) streams with
  `Range: bytes=0-`, seeks with `Range: bytes=<pos>-`, and probes the file size with
  `Range: bytes=<size-8>-<size-1>`.
- `Content-Type` is `text/plain; charset=UTF-8` for media (harmless quirk).
- Measured throughput: ~6.4 MB/s in the first connection log; a 47 MB clip is downloadable in
  seconds.

This is what makes partial cropping possible: GKUview locates a keyframe and the end of the
selection by timestamp (see [architecture.md](architecture.md)), then downloads one contiguous
byte range and stream-copies it locally.

## 5. Thumbnails

```
GET /app/getthumbnail?file=sd//Video/20261006_172024_F.TS
→ 200 image/jpeg, ~6 KB
```

Works for both cameras (`sd//Video_rear/…`) and for photos (`sd//Photo/…`). GKUview uses this
for list previews and falls back to local frame extraction for arbitrary timestamps.

## 6. GPS telemetry and privacy

`.TS` files carry an appended footer with telemetry. An observed footer:

- starts with `00 00 01 b0` followed by the ASCII marker `SKIPLIGOGPSINFO`,
- contains newline-separated GPS records with timestamp, latitude, longitude, speed,
  accelerometer axes,
- ends with a `####` marker.

The payload capture used to write this document therefore contains **location data** and is kept
out of the repository (`.gitignore` excludes `*.pcap`). GKUview never requests the footer: the
byte-range planner stops at the selection's end packet, so cropped exports contain only the
video/audio window.

## 7. Live view (RTSP)

```
DESCRIBE rtsp://192.168.0.1:554/livestream/12
```
```
m=video 0 RTP/AVP 98
a=framerate:30.0
a=control:trackID=0
a=rtpmap:98 H265/90000
a=x-dimensions:1280,720
```

- Front camera: `rtsp://192.168.0.1:554/livestream/12`, rear: `…/livestream/13`
  (URLs come from `getmediainfo`; no audio track in the SDP).
- Transport: RTP over TCP (interleaved).
- GKUview opens the stream in `mpv` or `ffplay`; embedding a decoder is on the roadmap.

## 8. How GKUview maps to the API

Defaults in `crates/gkuview-core/src/endpoints.rs`:

| Setting | Value |
| --- | --- |
| `list_url_template` | `/app/getfilelist?folder={folder}&start={start}&end={end}` |
| `folders` | `loop`, `event`, `emr`, `park`, `race` |
| `page_size` | `100` |
| `device_info_paths` | `/app/getproductinfo`, `/app/getdeviceattr` |
| `media_info_path` | `/app/getmediainfo` |
| `file_url_template` | `{base}/{path}` |
| `thumbnail_url_template` | `{base}/app/getthumbnail?file={path}` |
| `rtsp_url_template` | `rtsp://{host}:554/livestream/12` (fallback; `getmediainfo` wins) |
| `list_size_scale` | `1024` (listing sizes are KiB) |

The older generic candidates (`/?custom=1&cmd=3011…`, `/DCIM/100MEDIA/`) remain as a fallback
for other firmwares and are only probed when the template yields nothing.

## 9. Remaining unknowns

| Unknown | Notes |
| --- | --- |
| Port 7878 protocol | Advertised by `getmediainfo`, seen as a long-lived low-rate flow in the connection log, but absent from the payload capture. Not needed for browse/download/crop. |
| `mac`/`uuid`/`ssid` fields | Redacted here; not required to talk to the camera. |
| `getrecduration` semantics | Likely "seconds remaining in the current recording file", not clip duration. |
| Non-loop folder retention | `emr`/`event`/`park`/`race` behaviour varies by model and settings. |
| 4K models | Media codec may differ (H.265 profile/resolution); the TS/range logic is codec-agnostic. |
