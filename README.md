# Theia Rust

[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange)](https://www.rust-lang.org/)
[![MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

A portable media streaming server written in Rust. Streams any size video with low,
constant memory use, and **transcodes VP9/AV1/Opus (or anything) to H.264/AAC on the fly**
so it plays on devices that lack VP9/AV1 hardware decode — e.g. an older iPad. Perfect for
media libraries on NAS, local networks, or portable devices.

## 📥 Downloads

Download the latest binary from [Releases](https://github.com/weiyou/theia-rust/releases).

## ✅ Requirements

- **ffmpeg** and **ffprobe** on `PATH` (or set their paths in the config file). On macOS:
  `brew install ffmpeg`. Transcoding uses Apple `h264_videotoolbox` hardware encoding when
  available, so it stays fast and light on Apple Silicon.

## 🚀 Quick Install

```bash
cargo install --git https://github.com/weiyou/theia-rust
```

## 🎬 Features

- **On-Demand Transcoding**: A per-file **H264/AAC** button transcodes VP9/AV1/Opus and
  more to H.264/AAC HLS in real time (hardware-accelerated on Apple Silicon)
- **Wide Format Listing**: Lists `mp4, m4v, mov, webm, mkv, avi, ts, flv` (configurable)
- **Constant-Memory Streaming**: Streams any size file with full HTTP byte-range seeking
- **Directory Browsing**: Clean HTML interface with codec / resolution / size badges
- **Playlist Mode**: Auto-play all videos in a folder sequentially
- **Basic Auth**: Username/password protection (configurable), constant-time compare
- **Config File**: Optional TOML config for auth, port, root, codecs, and cache
- **Optional HTTPS**: Build with `--features tls` to serve over TLS
- **Cross-Platform**: Works on macOS, Linux, Windows, and more

## 📱 Usage

### Basic Usage

```bash
# Serve from default ~/Theia_Home directory
theia

# Serve from custom directory
theia --root /path/to/your/videos
```

Then open http://localhost:32450 in your browser. Use username `theia` and password `theia`.

### Kindle Fire Silk Browser

For Amazon Kindle Fire tablets:
1. Enable "Desktop Mode" in Silk browser settings
2. Visit http://your-server-ip:32450
3. Authenticate with theia/theia
4. Videos play with full controls and seeking

### iOS / iPadOS Safari (including older iPads without VP9/AV1 decode)

1. Visit http://your-server-ip:32450
2. Authenticate with theia/theia
3. Tap a file name to direct-stream, or tap the **▶ H264/AAC** button to play a
   real-time transcode (HLS). The transcode is what lets VP9/AV1/Opus files play on an
   iPad Pro (2nd gen) and similar devices.

### Configuration file

Optional TOML at `~/.config/theia/config.toml` (or `--config <path>`):

```toml
username = "theia"
password = "theia"
port = 32450
# root = "/path/to/media"          # default: ~/Theia_Home
ffmpeg = "ffmpeg"                   # or an absolute path
ffprobe = "ffprobe"
# cache_dir = "/path/to/cache"      # default: OS cache dir /theia/hls
cache_max_gb = 10.0                 # LRU + age cap for transcoded HLS caches (default 10 GB)
encoder = "h264_videotoolbox"       # or "libx264" for better compression
max_concurrent_transcodes = 2       # limit parallel ffmpeg jobs (P1 safeguard)
hls_segment_format = "ts"           # "ts" (legacy MPEG-TS, best compatibility with older devices like old iPads)
                                   # "fmp4" (modern fragmented MP4, better on recent clients & Apple Silicon)
extensions = ["mp4", "m4v", "mov", "webm", "mkv", "avi", "ts", "flv"]
```

CLI flags (`--root`, `--port`, `--tls-cert`, `--tls-key`) override the config file.

**Note on `hls_segment_format`**:
- `"ts"` (default): Uses classic MPEG-TS segments (`.ts`). Offers the widest compatibility, especially with older devices such as 2nd-generation iPads.
- `"fmp4"`: Uses modern fragmented MP4 segments (`.m4s`). Can provide slightly better performance and seeking on newer clients and Apple Silicon Macs, but may not play on some older hardware.

When using `h264_videotoolbox` (or other `*_videotoolbox` encoders) on Apple Silicon, Theia now enables hardware decoding and improved rate control (with headroom) for significantly lower CPU usage. See GitHub issue #8 for details.

### Debugging Transcode Failures
If a video fails to play via the H264/AAC button, you can inspect the raw `ffmpeg` output:
- Call `GET /hls/{enc}/ffmpeg.log` (authenticated) — this returns the log file written during the transcode attempt.
- The server also logs the tail of the ffmpeg log at ERROR level on failure (visible if `RUST_LOG=theia=error` or similar).

This greatly improves observability for issues like unsupported codecs, missing streams, or encoder problems (GitHub issue #5).

## 🏗️ Development

### Prerequisites

- Rust 1.70 or later
- Cargo package manager

### Build from Source

```bash
git clone https://github.com/weiyou/theia-rust
cd theia-rust
cargo build --release
```

### Run Tests

```bash
cargo test
```

For the full manual verification matrix (including the new P0 stale-cache behavior and HLS end-to-end),
see the **✅ Verification** section above. `cargo test` now also covers the manifest round-trip and
invalidation logic added after the v0.3 evaluation.
### Project Structure

```
theia-rust/
├── src/
│   ├── main.rs        # CLI, config load, router wiring, startup
│   ├── config.rs      # TOML config + CLI merge
│   ├── auth.rs        # basic-auth middleware + login page
│   ├── scan.rs        # media scan, path encode/decode, traversal containment
│   ├── listing.rs     # directory HTML + "Play All" playlist
│   ├── stream.rs      # direct byte-range streaming
│   ├── probe.rs       # ffprobe metadata (cached) + /meta endpoint
│   ├── transcode.rs   # on-demand HLS transcoding + cache sweeper
│   └── player.rs      # HLS player page
├── Cargo.toml
├── docs/PLAN.md       # Living architecture document
├── docs/original-theia.swift
├── LICENSE
└── README.md
```

## 📚 API Endpoints

- `GET /` - Directory listing with media tree and per-file H264/AAC buttons
- `GET /login` - Authentication instructions page
- `GET /stream/{enc}` - Direct-stream a media file with byte-range support
- `GET /meta/{enc}` - JSON codec/resolution/size metadata (ffprobe, cached)
- `GET /play/{enc}` - Player page that plays the file as transcoded H.264/AAC HLS
- `GET /hls/{enc}/index.m3u8` - On-demand HLS playlist (starts/attaches a transcode)
- `GET /hls/{enc}/seg-NNNNN.ts` (or .m4s) - Transcoded HLS segment
- `GET /hls/{enc}/ffmpeg.log` - Raw ffmpeg log for debugging transcode failures (auth required)
- `GET /status` - JSON status of active transcodes and concurrency limit (auth required)
- `GET /playall/{enc}` - Auto-play (direct-stream) playlist for a folder

## ✅ Verification

### Automated Tests
```bash
cargo test
```
This covers path safety, segment name validation, bitrate ladder, and (since the v0.3 evaluation)
the new P0 cache manifest + stale source invalidation logic in `transcode.rs`.

### Manual End-to-End Matrix (recommended before releases)

Use a clean `~/Theia_Home` (or `--root /tmp/theia-test`) and `ffmpeg` on PATH.

1. **Happy Path (core feature)**
   - Add a short VP9, AV1, or Opus-containing file.
   - Visit `http://localhost:32450`, authenticate (`theia`/`theia` by default).
   - Click the **▶ H264/AAC** button on a non-H.264 file.
   - Verify: Safari/iOS plays the HLS stream, audio works, video is H.264 High (check stats or `ffprobe` on a captured segment), seeking within the already-produced prefix works.
   - Second load of the same file is instant (cached playlist, no new ffmpeg).

2. **Cache Invalidation (P0 fix, issue #3)**
   - Let a transcode complete (manifest.json + segments written in `~/.cache/theia/hls/<hash>`).
   - Replace the source in-place: `cp new-version.mkv original.mkv` (or edit content + `touch`).
   - Reload the H264/AAC player for that file.
   - Expected: old cache is removed, a fresh transcode starts (new manifest with updated mtime/size).

3. **Seeking & Long-Form Behavior**
   - Use a longer source (> 10 min).
   - Play, then seek far forward (e.g. 80%).
   - Note the buffering time — this demonstrates the current linear "event" playlist limitation (ffmpeg must reach that point). See issue #6 for the planned segment-on-demand improvement.

4. **Lifecycle & Resources**
   - Start several H264/AAC transcodes.
   - Stop watching → after ~120 s idle, `ps | grep ffmpeg` should show the processes reaped.
   - Fill the cache past `cache_max_gb` (or set it low) → the sweeper should evict oldest completed directories while protecting active sessions.

5. **Safety**
   - Try `../../etc/passwd` or malicious segment names (`seg-../../../etc/passwd.ts`) → rejected (400/404, no escape).
   - Wrong Basic Auth → 401 with `WWW-Authenticate`.

6. **Error Cases**
   - Feed a corrupt or stream-less file → useful(ish) error surfaced; `ffmpeg.log` inside the (now-invalidated) cache dir contains the real ffmpeg output.

See GitHub issues #3–#7 and `docs/PLAN.md` for the full post-evaluation findings and roadmap.

## 🔧 Configuration

### CLI Options

- `--root <PATH>` - Root directory to serve files from (default: ~/Theia_Home)
- `--config <PATH>` - Config file path (default: ~/.config/theia/config.toml)
- `--port <PORT>` - Listen port (default: 32450)
- `--tls-cert <PATH>` / `--tls-key <PATH>` - PEM cert/key (requires `--features tls`)
- `--max-transcodes <N>` - Maximum concurrent transcodes (overrides config). Default: 2
- `--help` - Show help information

### Environment Variables

- `RUST_LOG` - Optional log filter (e.g. `theia=info,tower_http=debug`)

## 🤝 Contributing

1. Fork the repository
2. Create a feature branch: `git checkout -b feature-name`
3. Make your changes
4. Run tests: `cargo test`
5. Submit a pull request

## 📄 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## 🙏 Acknowledgments

- Original Swift implementation for inspiration
- Axum framework for excellent HTTP server capabilities
- Rust community for outstanding tooling

## 🔮 Future Features

See [PLAN.md](docs/PLAN.md) for the living architecture document (updated after the
v0.3 HLS transcoding evaluation) and the full roadmap. Key follow-up work is tracked
in GitHub issues:
- [#3](https://github.com/weiyou/theia-rust/issues/3) — P0 stale cache invalidation + source manifest (completed in this prototype)
- [#6](https://github.com/weiyou/theia-rust/issues/6) — Segment-on-demand for instant arbitrary seeking
- [#4](https://github.com/weiyou/theia-rust/issues/4), [#5](https://github.com/weiyou/theia-rust/issues/5), [#7](https://github.com/weiyou/theia-rust/issues/7) — concurrency limits, error visibility, and testing/CI improvements

Planned enhancements also include:
- Transcoded "Play All" for folders
- Smarter per-file "Play" vs "H264/AAC" buttons using probe data
- File upload / search / bulk operations
- And more!