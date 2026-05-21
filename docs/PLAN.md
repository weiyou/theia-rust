**Cline Instruction**: Before ANY /act or /deep-planning, always read and strictly follow @docs/PLAN.md. This is the living architecture document. Never deviate without updating it first.

# Implementation Plan - Current State & Future Features

[Overview]
Theia is a portable media streaming server in Rust (Axum + Tokio). It serves a directory of
media files with a browsable HTML tree, constant-memory byte-range streaming, a per-folder
"Play All" playlist, and basic auth. Its headline capability is **on-demand transcoding**:
a per-file **H264/AAC** button transcodes any source (notably VP9/AV1/Opus) to H.264/AAC and
delivers it as **HLS**, which Safari plays natively with seeking. This is what lets
VP9/AV1 content play on devices without VP9/AV1 hardware decode (e.g. a 2nd-gen iPad Pro).
Transcoding shells out to `ffmpeg`/`ffprobe`; on Apple Silicon it uses the
`h264_videotoolbox` hardware encoder, which runs faster than realtime.

[Architecture / Files]
Code is split into focused modules under `src/`:

- `main.rs` — module wiring, `AppState`, router, startup, plain/TLS serving.
- `config.rs` — `Args` (clap) + `FileConfig` (TOML) merged into a resolved `Config`
  (precedence CLI > config file > defaults). `Config::is_media()` gates the extension set.
- `auth.rs` — `basic_auth_middleware` (config creds, constant-time compare via `subtle`),
  exempts `/login`; `login_handler`.
- `scan.rs` — `MediaNode` tree, `encode_path`/`decode_path`, `scan_media`, and
  `resolve_within` (canonicalize + containment check that blocks `../` and absolute paths).
- `listing.rs` — directory HTML (escaped names, size, lazy `/meta` badges, H264/AAC button)
  and `playall_handler` (direct-stream playlist).
- `stream.rs` — `stream_handler`: byte-range streaming in a fixed 256KB buffer; content type
  by extension.
- `probe.rs` — `Probe` struct, mtime-keyed cache, `probe()` (ffprobe JSON), `meta_handler`.
- `transcode.rs` — `TranscodeManager`, HLS session lifecycle, `hls_handler`
  (playlist + segment), bitrate selection, `is_segment_name` guard, and `spawn_sweeper`
  (idle-session reaping + LRU cache eviction by `cache_max_bytes`).
- `player.rs` — `play_handler`: full-screen HLS player (native Safari, hls.js fallback).

[State]
`AppState { config: Arc<Config>, probe_cache: ProbeCache, transcoder: TranscodeManager }`.
`ProbeCache = Arc<Mutex<HashMap<PathBuf,(SystemTime,Probe)>>>`.
`TranscodeManager` holds `Arc<tokio::Mutex<HashMap<String, Session>>>` keyed by a hash of the
canonical file path; each `Session` owns the ffmpeg `Child` (`kill_on_drop`) + `last_access`.

[Routes]
- `GET /` directory listing · `GET /login`
- `GET /stream/{enc}` direct byte-range stream
- `GET /meta/{enc}` cached ffprobe JSON
- `GET /play/{enc}` HLS player page
- `GET /hls/{enc}/index.m3u8` start/attach transcode, return playlist
- `GET /hls/{enc}/seg-NNNNN.ts` transcoded segment
- `GET /playall/{enc}` direct-stream folder playlist

[Transcode pipeline]
ffmpeg writes an `event` HLS playlist + TS segments to `cache_dir/<hash>/`:
`-c:v <encoder> -profile:v high -b:v <bitrate-by-height> -tag:v avc1 -c:a aac -b:a 160k
-ac 2 -f hls -hls_time 6 -hls_flags independent_segments -hls_playlist_type event`.
Playlist requests poll until the first `.ts` appears; segments are served from the cache dir
(name strictly validated). Hardware encode outpaces playback, so seeking works in practice.

[Dependencies]
axum, tokio (full), walkdir, clap, dirs, base64, urlencoding, async-stream, serde,
serde_json, toml, tracing, tracing-subscriber, tower-http (trace), html-escape, subtle.
Optional `tls` feature: axum-server (tls-rustls). Runtime: ffmpeg + ffprobe on PATH.

[Testing]
Unit tests: path encode/decode + traversal containment (`scan.rs`), segment-name validation
and bitrate selection (`transcode.rs`). Manual end-to-end verified with generated
VP9/AV1/H264 samples (listing, /meta, range, /hls playlist+segment codecs, traversal block,
HTTPS). See README "Verification".

## Next Possible Features (Priority Order)

1. **Segment-on-demand transcoding**: compute a VOD playlist from duration up front and
   transcode each requested segment with `-ss`, for instant arbitrary seeking on huge files.
2. **Transcoded "Play All"**: switch the folder playlist to per-file HLS so incompatible
   codecs auto-play in sequence on the iPad.
3. **Smart default button**: use `Probe::ipad_native()` to label the button (Play vs
   Transcode) per file.
4. **File upload / deletion / rename / move**: write operations with confirmation.
5. **Search & sorting**: `/search?q=` and sort query params.
6. **Pagination** for very large directories.
7. **Rate limiting** and richer structured logging.
8. **Bulk operations**: multi-select batch actions.
