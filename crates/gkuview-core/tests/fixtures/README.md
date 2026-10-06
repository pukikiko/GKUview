# Test fixtures

Small MP4 files used by the integration tests. They are built from FFmpeg's synthetic sources so
the tests never need a real dashcam recording.

Regenerate with:

```bash
ffmpeg -y -f lavfi -i testsrc2=size=160x120:rate=10:duration=4 \
       -f lavfi -i sine=frequency=440:sample_rate=44100:duration=4 \
       -c:v libopenh264 -pix_fmt yuv420p -g 10 -c:a aac -b:a 64k \
       -movflags +faststart sample_faststart.mp4

ffmpeg -y -f lavfi -i testsrc2=size=160x120:rate=10:duration=4 \
       -f lavfi -i sine=frequency=440:sample_rate=44100:duration=4 \
       -c:v libopenh264 -pix_fmt yuv420p -g 10 -c:a aac -b:a 64k \
       sample_moov_end.mp4
```

Regenerate the MPEG-TS fixtures (GKU recordings are H.265 in MPEG-TS; H.264 is fine for
parser tests):

```bash
ffmpeg -y -f lavfi -i testsrc2=size=160x120:rate=10:duration=4 \
       -c:v libopenh264 -pix_fmt yuv420p -g 10 -f mpegts sample.ts

ffmpeg -y -f lavfi -i testsrc2=size=160x120:rate=10:duration=4 \
       -c:v libopenh264 -pix_fmt yuv420p -g 10 -output_ts_offset 100 \
       -f mpegts sample_pts_offset.ts
```

- `sample_faststart.mp4` — `moov` before `mdat` (progressive download layout).
- `sample_moov_end.mp4` — `moov` after `mdat` (common on dashcams; exercises the tail scan).
- `sample.ts` — MPEG-TS with PTS starting near zero.
- `sample_pts_offset.ts` — MPEG-TS whose PTS start at 100 s; verifies that trimming uses
  file-relative seek offsets.

If your FFmpeg lacks `libopenh264`, substitute any H.264 encoder (`libx264`, `h264_vaapi`, …).
The parser is codec-agnostic; the tests only rely on timings, sample counts and keyframes.
