**Cline Instruction**: Before ANY /act or /deep-planning, always read and strictly follow @docs/implementation_plan.md. This is the living architecture document. Never deviate without updating it first.

# Theia Rust

[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange)](https://www.rust-lang.org/)
[![MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

A portable, single-binary MP4 streaming server written in Rust. Streams any size video without memory limits using 256MB chunked streaming. Perfect for media libraries on NAS, local networks, or portable devices.

## 📥 Downloads

| Platform              | File name                                      | Direct link |
|-----------------------|------------------------------------------------|-------------|
| macOS Apple Silicon   | theia-v0.2.0-aarch64-apple-darwin              | [Download](https://github.com/weiyou/theia-rust/releases/download/v0.2.0/theia-v0.2.0-aarch64-apple-darwin) |
| macOS Intel           | theia-v0.2.0-x86_64-apple-darwin               | [Download](https://github.com/weiyou/theia-rust/releases/download/v0.2.0/theia-v0.2.0-x86_64-apple-darwin) |
| Linux                 | theia-v0.2.0-x86_64-unknown-linux-gnu          | [Download](https://github.com/weiyou/theia-rust/releases/download/v0.2.0/theia-v0.2.0-x86_64-unknown-linux-gnu) |
| Windows               | theia-v0.2.0-x86_64-pc-windows-msvc.exe        | [Download](https://github.com/weiyou/theia-rust/releases/download/v0.2.0/theia-v0.2.0-x86_64-pc-windows-msvc.exe) |

> **Note**: GitHub does not autodetect your OS — choose the file for your platform.

## 🚀 Quick Install

```bash
cargo install --git https://github.com/weiyou/theia-rust
```

Or download the latest binary from [Releases](https://github.com/weiyou/theia-rust/releases).

## 🎬 Features

- **Unlimited Size Streaming**: Fixed 2GB limit - now streams any size MP4 using 256MB chunks
- **Directory Browsing**: Clean HTML interface for navigating your media library
- **Playlist Mode**: Auto-play all videos in a folder sequentially
- **Range Support**: Full HTTP byte-range support for seeking and resuming
- **Basic Auth**: Secure access with username/password protection
- **Cross-Platform**: Works on Windows, macOS, Linux, and more
- **Single Binary**: No dependencies, just one executable file
- **Portable**: Run from USB drive or any directory

## 📱 Usage

### Basic Usage

```bash
# Serve from default ~/Theia_Home directory
theia-rust

# Serve from custom directory
theia-rust --root /path/to/your/videos
```

Then open http://localhost:32450 in your browser. Use username `theia` and password `theia`.

### Kindle Fire Silk Browser

For Amazon Kindle Fire tablets:
1. Enable "Desktop Mode" in Silk browser settings
2. Visit http://your-server-ip:32450
3. Authenticate with theia/theia
4. Videos play with full controls and seeking

### iOS Safari

For iPhone/iPad Safari:
1. Visit http://your-server-ip:32450
2. Authenticate with theia/theia
3. Videos stream smoothly with native controls

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
├── src/main.rs          # Complete server implementation
├── Cargo.toml           # Dependencies and metadata
├── docs/implementation_plan.md # Technical documentation
├── docs/original-theia.swift # Original Swift implementation
├── LICENSE              # MIT license
└── README.md            # This file
```

## 📚 API Endpoints

- `GET /` - Directory listing with media tree
- `GET /login` - Authentication instructions page
- `GET /stream/{encoded-path}` - Stream MP4 file with range support
- `GET /playall/{encoded-folder}` - Auto-play playlist for folder

## 🔧 Configuration

### Environment Variables

- None required - everything configured via CLI flags

### CLI Options

- `--root <PATH>` - Root directory to serve files from (default: ~/Theia_Home)
- `--help` - Show help information

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

See [implementation_plan.md](docs/implementation_plan.md) for planned enhancements including:
- File upload support
- Search functionality
- HTTPS support
- Bulk operations
- And more!