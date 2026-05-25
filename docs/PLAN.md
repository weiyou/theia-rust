**Cline Instruction**: Before ANY /act or /deep-planning, always read and strictly follow @docs/PLAN.md. This is the living architecture document. Never deviate without updating it first.

# Implementation Plan - Current State & Future Features

[Overview]
Theia is a portable media streaming server in Rust (Axum + Tokio). It serves a directory of
media files with a browsable HTML tree, constant-memory byte-range streaming, a per-folder
"Play All" playlist, and basic auth. Its headline capability (landed in v0.3 on the
`feat/theia-hls-transcoding` branch) is **on-demand transcoding**:
a per-file **H264/AAC** button transcodes any source (notably VP9/AV1/Opus) to H.264/AAC and
delivers it as **HLS**, which Safari plays natively with seeking. This is what lets
VP9/AV1 content play on devices without VP9/AV1 hardware decode (e.g. a 2nd-gen iPad Pro).
Transcoding shells out to `ffmpeg`/`ffprobe`; on Apple Silicon it uses the
`h264_videotoolbox` hardware encoder, which runs faster than realtime.

> **2026 Evaluation Note**: A full code review of the v0.3 transcoding implementation
> (Grok 4.3) is captured in the session plan and resulted in GitHub issues #3–#7.
> The current design uses a linear "event" playlist (one ffmpeg from t=0 per file).
> This delivers excellent "play from start" UX but has known limitations for arbitrary
> seeking on long files and cache invalidation. See the new "Current Status & Known Gaps"
> section below.

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
- `GET /hls/{enc}/seg-NNNNN.ts` (or .m4s) transcoded segment
- `GET /playall/{enc}` direct-stream folder playlist
- `GET /status` — live view of active transcodes and concurrency (new)

[Transcode pipeline]
ffmpeg writes an `event` HLS playlist + TS segments to `cache_dir/<hash>/`:
`-c:v <encoder> -profile:v high -b:v <bitrate-by-height> -tag:v avc1 -c:a aac -b:a 160k
-ac 2 -f hls -hls_time 6 -hls_flags independent_segments -hls_playlist_type event`.
Playlist requests poll until the first `.ts` appears; segments are served from the cache dir
(name strictly validated).

**Known limitation (v0.3)**: This is a *linear* transcode from t=0. The playlist only
contains segments that ffmpeg has already produced. Seeking works well within the
already-transcoded prefix (especially with fast HW encoding), but arbitrary seeks on
long files may require waiting for ffmpeg to reach that point. True segment-on-demand
is tracked as a future increment (see issues and roadmap below).

A lightweight `manifest.json` (source mtime/size + probe data) is being added as part
of the P0 cache-correctness work so that stale transcodes are invalidated when the
source file changes.

[Dependencies]
axum, tokio (full), walkdir, clap, dirs, base64, urlencoding, async-stream, serde,
serde_json, toml, tracing, tracing-subscriber, tower-http (trace), html-escape, subtle.
Optional `tls` feature: axum-server (tls-rustls). Runtime: ffmpeg + ffprobe on PATH.

[Testing]
Unit tests: path encode/decode + traversal containment (`scan.rs`); segment-name validation,
bitrate selection, manifest round-trip + stale-source detection, VOD playlist generation, and
the concurrency-limit semaphore (`transcode.rs`).

Router integration tests (`src/main.rs` `#[cfg(test)]`): drive the real `build_app` router via
`tower::oneshot` to verify auth enforcement (401 / `WWW-Authenticate`), the `/login` exemption,
wrong-password rejection, and an authenticated directory listing — no socket or ffmpeg required,
so they run anywhere. (Bin-only crate, so these live in-crate rather than under `tests/`.)

A basic CI workflow runs `cargo test + clippy` on PRs and pushes (see `.github/workflows/ci.yml`).
An ffmpeg-backed smoke test that exercises a real HLS transcode end-to-end is the remaining
stretch goal in issue #7. See the "✅ Verification" section in README.md for the manual test matrix.

## Current Status & Known Gaps (Post-v0.3 Evaluation)

The `feat/theia-hls-transcoding` branch successfully delivered the core "H264/AAC button"
experience. A detailed code review (Grok 4.3, 2026) identified one **critical correctness bug**
and several robustness/UX gaps. Work is tracked in the following GitHub issues:

- **#3 (P0 — Critical)**: Stale HLS transcode cache on source file replacement / mtime change.
  The cache must be invalidated when the source changes (add `manifest.json` with mtime/size + probe data).
- **#8 (P0 — High Impact)**: Major Apple Silicon (M4+) transcoding improvements — hardware decode via `-hwaccel videotoolbox`, improved rate control with headroom (dynamic `-maxrate` + `-bufsize`, `-qmin`/`-qmax`), `-realtime` mode, and optional modern fMP4 (`.m4s`) output. Delivers significantly lower CPU usage and better consistency on M-series Macs.
- **#4 (P1)**: No limit on concurrent transcodes → resource exhaustion risk on NAS / multi-user.
- **#5 (P1)**: Transcode errors are opaque (no way to surface `ffmpeg.log` or actionable messages).
- **#6 (P2 / Architecture)**: Segment-on-demand transcoding for instant arbitrary seeking on long files.
  **Status: open / not implemented.** Groundwork is in place — a `transcode_mode` config flag
  (`"linear"` | `"segment"`) and a unit-tested `generate_vod_playlist()` helper — but the actual
  on-demand per-segment transcode path is not built. Selecting `"segment"` today logs a warning
  and falls back to linear. A correct implementation still needs: per-segment ffmpeg with proper
  timestamp offset (`-output_ts_offset`/`-copyts`) and keyframe alignment, the per-segment work
  guarded by the concurrency semaphore (#4), playlist caching + stale-cache integration (#3),
  and fMP4 (#8) support. See the issue for the design.
- **#7**: Testing, CI, and docs gaps for the transcoding feature (integration coverage, PR smoke jobs,
  reproducible verification steps in README).

See the session evaluation plan for the full analysis, repro steps, and recommended implementation order.

## Next Possible Features (Living Roadmap)

### High-Priority Transcoding Evolution
1. **Apple Silicon (M4+) ffmpeg improvements** (#8): Hardware decode (`-hwaccel videotoolbox`), dynamic rate control with headroom, `-realtime` mode, and optional fMP4 output. One of the highest-impact performance wins on M-series Macs (dramatically lower CPU usage).
2. **Segment-on-demand transcoding** (see #6): **Not yet implemented.** Groundwork only —
   the `transcode_mode = "segment"` config flag and a unit-tested `generate_vod_playlist()`
   helper exist; selecting the mode currently warns and falls back to linear.
   - To build: serve a complete VOD playlist upfront, then transcode each requested segment
     on demand via a short ffmpeg `-ss` invocation, with correct timestamp offsets
     (`-output_ts_offset`/`-copyts`) and keyframe alignment so segments line up with the playlist.
   - Each on-demand transcode must run under the concurrency semaphore (#4), and the path must
     integrate with stale-cache invalidation (#3) and fMP4 output (#8).
3. **Cache correctness + manifest** (#3): Lightweight `manifest.json` per cache dir.

### UX Polish
3. **Transcoded "Play All"**: Switch the folder playlist to per-file HLS so incompatible codecs
   auto-play in sequence on the iPad (and other limited clients).
4. **Smart default button**: Use `Probe::ipad_native()` to label the button ("▶ Play" vs "▶ H264/AAC")
   and choose the default action per file.

### Platform & Operations
5. **Concurrent transcode limiting + backpressure** (#4): `max_concurrent_transcodes` config + semaphore.
6. **Better error observability** (#5): Expose `ffmpeg.log` via `/hls/{enc}/ffmpeg.log`, surface useful
   messages on failure, structured tracing events for session lifecycle.
7. **Improved cache LRU**: Persisted last-access tracking instead of (or in addition to) filesystem
   atime/mtime for more reliable eviction of completed transcodes.

### General Features
8. **File upload / deletion / rename / move**: Write operations with confirmation.
9. **Search & sorting**: `/search?q=` and sort query params.
10. **Pagination** for very large directories.
11. **Rate limiting** and richer structured logging / admin status page.
12. **Bulk operations**: multi-select batch actions.

See the individual GitHub issues for detailed acceptance criteria and design sketches. The P0 item
(#3) should be completed before any wider v0.3.x release on real media libraries.
