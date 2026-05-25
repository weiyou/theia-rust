//! Runtime configuration: defaults < TOML config file < CLI flags.
use clap::Parser;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// CLI arguments. Anything left unset falls back to the config file, then defaults.
#[derive(Parser)]
#[command(name = "theia")]
#[command(about = "Portable media streaming server with on-demand H264/AAC transcoding")]
pub struct Args {
    /// Root directory to serve files from (default: ~/Theia_Home)
    #[arg(short, long)]
    pub root: Option<PathBuf>,

    /// Path to a TOML config file (default: ~/.config/theia/config.toml)
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Port to listen on (default: 32450)
    #[arg(short, long)]
    pub port: Option<u16>,

    /// TLS certificate (PEM). Requires building with --features tls and --tls-key.
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,

    /// TLS private key (PEM). Requires building with --features tls and --tls-cert.
    #[arg(long)]
    pub tls_key: Option<PathBuf>,

    /// Maximum concurrent transcodes (overrides config file). Default: 4
    #[arg(long = "max-transcodes")]
    pub max_transcodes: Option<usize>,
}

/// On-disk config schema. Every field is optional via `#[serde(default)]`.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct FileConfig {
    username: String,
    password: String,
    port: u16,
    root: Option<PathBuf>,
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    cache_dir: Option<PathBuf>,
    cache_max_gb: f64,
    encoder: String,
    extensions: Vec<String>,
    max_concurrent_transcodes: usize,
    hls_segment_format: String,
    /// Future: "linear" (current event playlist from t=0) or "segment" (on-demand per-segment)
    transcode_mode: String,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            username: "theia".to_string(),
            password: "theia".to_string(),
            port: 32450,
            root: None,
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            cache_dir: None,
            cache_max_gb: 10.0,
            encoder: "h264_videotoolbox".to_string(),
            extensions: ["mp4", "m4v", "mov", "webm", "mkv", "avi", "ts", "flv"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            max_concurrent_transcodes: 4, // default raised to 4 thanks to hardware decode on Apple Silicon
            hls_segment_format: "ts".to_string(), // "ts" = legacy MPEG-TS (best compatibility), "fmp4" = modern fragmented MP4
            transcode_mode: "linear".to_string(), // "linear" (current) or "segment" (future on-demand)
        }
    }
}

/// Fully resolved configuration handed to the running server.
#[derive(Debug, Clone)]
pub struct Config {
    pub username: String,
    pub password: String,
    pub port: u16,
    pub root: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub cache_dir: PathBuf,
    pub cache_max_bytes: u64,
    pub encoder: String,
    pub extensions: Vec<String>,
    /// Maximum number of concurrent ffmpeg transcodes allowed.
    /// Additional requests queue (fairly) until a slot is free.
    /// Default: 4 (raised thanks to efficient hardware decode + encode on Apple Silicon).
    pub max_concurrent_transcodes: usize,
    /// "ts" (legacy MPEG-TS, best compatibility with older devices)
    /// or "fmp4" (modern fragmented MP4, better on recent clients & Apple Silicon)
    pub hls_segment_format: String,
    /// "linear" (current event playlist model) or "segment" (future per-segment on-demand)
    pub transcode_mode: String,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
}

impl Config {
    /// Load config from the chosen TOML file (if any) and overlay CLI flags.
    pub fn load(args: Args) -> Self {
        let config_path = args.config.clone().unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("theia")
                .join("config.toml")
        });

        let file = match std::fs::read_to_string(&config_path) {
            Ok(contents) => match toml::from_str::<FileConfig>(&contents) {
                Ok(parsed) => {
                    eprintln!("Loaded config from {}", config_path.display());
                    parsed
                }
                Err(e) => {
                    eprintln!("Warning: could not parse {}: {e}", config_path.display());
                    FileConfig::default()
                }
            },
            Err(_) => FileConfig::default(),
        };

        let root = args
            .root
            .or(file.root)
            .unwrap_or_else(|| home_join("Theia_Home"));

        let cache_dir = file.cache_dir.unwrap_or_else(|| {
            dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("theia")
                .join("hls")
        });

        Self {
            username: file.username,
            password: file.password,
            port: args.port.unwrap_or(file.port),
            root,
            ffmpeg: file.ffmpeg,
            ffprobe: file.ffprobe,
            cache_dir,
            cache_max_bytes: (file.cache_max_gb * 1024.0 * 1024.0 * 1024.0) as u64,
            encoder: file.encoder,
            extensions: file.extensions.iter().map(|e| e.to_lowercase()).collect(),
            max_concurrent_transcodes: args.max_transcodes.unwrap_or(file.max_concurrent_transcodes),
            hls_segment_format: file.hls_segment_format.to_lowercase(),
            transcode_mode: file.transcode_mode.to_lowercase(),
            tls_cert: args.tls_cert,
            tls_key: args.tls_key,
        }
    }

    /// True if `path` has an extension in the configured media set.
    pub fn is_media(&self, path: &Path) -> bool {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => {
                let ext = ext.to_lowercase();
                self.extensions.iter().any(|e| e == &ext)
            }
            None => false,
        }
    }

    /// Returns true if we should output modern fragmented MP4 segments (.m4s)
    /// instead of legacy MPEG-TS (.ts). "fmp4" generally offers slightly better
    /// compatibility on modern devices and better performance on Apple Silicon.
    pub fn uses_fmp4_segments(&self) -> bool {
        self.hls_segment_format == "fmp4"
    }

    /// Whether we are in the future per-segment on-demand mode (vs current linear event mode).
    pub fn is_segment_on_demand(&self) -> bool {
        self.transcode_mode == "segment"
    }
}

fn home_join(child: &str) -> PathBuf {
    dirs::home_dir()
        .expect("Could not find home directory")
        .join(child)
}

#[cfg(test)]
impl Config {
    /// Minimal config for integration tests.
    pub fn test_default(root: PathBuf) -> Self {
        Self {
            username: "test".to_string(),
            password: "test".to_string(),
            port: 0,
            root,
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            cache_dir: std::env::temp_dir().join("theia-test-cache"),
            cache_max_bytes: 100 * 1024 * 1024, // 100MB for tests
            encoder: "h264_videotoolbox".to_string(),
            extensions: vec!["mp4".to_string(), "mkv".to_string()],
            max_concurrent_transcodes: 1,
            hls_segment_format: "ts".to_string(),
            transcode_mode: "linear".to_string(),
            tls_cert: None,
            tls_key: None,
        }
    }
}
