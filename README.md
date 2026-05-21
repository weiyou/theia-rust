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
cache_max_gb = 20.0                 # LRU cap for transcoded HLS segments
encoder = "h264_videotoolbox"       # or "libx264" for better compression
extensions = ["mp4", "m4v", "mov", "webm", "mkv", "avi", "ts", "flv"]
```

CLI flags (`--root`, `--port`, `--tls-cert`, `--tls-key`) override the config file.

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
- `GET /hls/{enc}/seg-NNNNN.ts` - Transcoded HLS segment
- `GET /playall/{enc}` - Auto-play (direct-stream) playlist for a folder

## 🔧 Configuration

### CLI Options

- `--root <PATH>` - Root directory to serve files from (default: ~/Theia_Home)
- `--config <PATH>` - Config file path (default: ~/.config/theia/config.toml)
- `--port <PORT>` - Listen port (default: 32450)
- `--tls-cert <PATH>` / `--tls-key <PATH>` - PEM cert/key (requires `--features tls`)
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

See [PLAN.md](docs/PLAN.md) for the living architecture document and planned enhancements
including:
- Segment-on-demand transcoding for instant arbitrary seeking
- File upload, search, and bulk operations
- Per-folder "Play All" via transcoded HLS
- And more!