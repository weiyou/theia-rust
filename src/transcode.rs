//! On-demand HLS transcoding: VP9/AV1/Opus (or anything) -> H.264/AAC.
//!
//! When a client requests `/hls/{enc}/index.m3u8` we spawn one ffmpeg process that
//! decodes the source and writes HLS TS segments + a growing (event) playlist into a
//! per-file cache directory. Safari plays the playlist natively. On the M4 the
//! hardware `h264_videotoolbox` encoder runs faster than realtime, so segments are
//! produced ahead of playback and seeking works across the whole timeline.
use crate::config::Config;
use crate::probe::{self, ProbeCache};
use crate::scan::{decode_path, resolve_within};
use crate::AppState;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path as StdPath, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const SEGMENT_SECONDS: u32 = 6;
/// Kill a transcode session that has had no requests for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long to wait for ffmpeg to produce the first playlist entry.
const PLAYLIST_WAIT: Duration = Duration::from_secs(20);

struct Session {
    dir: PathBuf,
    /// Held so the process is killed on drop (`kill_on_drop`).
    _child: Child,
    last_access: Instant,
}

#[derive(Clone)]
pub struct TranscodeManager {
    config: Arc<Config>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    probe_cache: ProbeCache,
}

impl TranscodeManager {
    pub fn new(config: Arc<Config>, probe_cache: ProbeCache) -> Self {
        Self {
            config,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            probe_cache,
        }
    }
}

/// Deterministic cache key (and directory name) for a canonical file path.
fn key_for(path: &StdPath) -> String {
    let mut h = DefaultHasher::new();
    path.to_string_lossy().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Pick a target video bitrate from the source height.
fn bitrate_for(height: u32) -> &'static str {
    match height {
        h if h >= 2160 => "12M",
        h if h >= 1440 => "9M",
        h if h >= 1080 => "6M",
        h if h >= 720 => "3M",
        h if h >= 480 => "1500k",
        _ => "900k",
    }
}

/// `GET /hls/{enc}/{seg}` — serves either the playlist or a TS segment.
pub async fn hls_handler(
    Path((encoded_path, seg)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let decoded = match decode_path(&encoded_path) {
        Some(d) => d,
        None => return (StatusCode::BAD_REQUEST, "Invalid path encoding").into_response(),
    };
    let file = match resolve_within(&state.config.root, &decoded) {
        Some(p) if state.config.is_media(&p) && p.is_file() => p,
        _ => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };

    if seg == "index.m3u8" {
        serve_playlist(&state, &file).await
    } else if is_segment_name(&seg) {
        serve_segment(&state, &file, &seg).await
    } else {
        (StatusCode::BAD_REQUEST, "Invalid segment name").into_response()
    }
}

/// Only allow `seg-NNNNN.ts` so the segment name can't escape the cache dir.
fn is_segment_name(seg: &str) -> bool {
    seg.strip_prefix("seg-")
        .and_then(|s| s.strip_suffix(".ts"))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

async fn serve_playlist(state: &AppState, file: &StdPath) -> Response {
    let mgr = &state.transcoder;
    let key = key_for(file);
    let dir = mgr.config.cache_dir.join(&key);
    let playlist = dir.join("index.m3u8");

    // Fast path: an existing session (or a completed cache) already has the playlist.
    {
        let mut sessions = mgr.sessions.lock().await;
        if let Some(session) = sessions.get_mut(&key) {
            session.last_access = Instant::now();
        } else if playlist.exists() {
            // Completed transcode from a previous run — serve from cache, no respawn.
            return read_playlist(&playlist).await;
        } else {
            // Start a new session while holding the lock to avoid double-spawn.
            match start_session(&mgr.config, &mgr.probe_cache, file, &dir).await {
                Ok(session) => {
                    sessions.insert(key.clone(), session);
                }
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to start transcoder: {e}"),
                    )
                        .into_response();
                }
            }
        }
    }

    // Wait for ffmpeg to write the first segment into the playlist.
    let deadline = Instant::now() + PLAYLIST_WAIT;
    loop {
        if let Ok(contents) = tokio::fs::read_to_string(&playlist).await
            && contents.contains(".ts")
        {
            return playlist_response(contents);
        }
        if Instant::now() >= deadline {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Transcoder did not produce output in time (check the source codecs / ffmpeg log)",
            )
                .into_response();
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn read_playlist(playlist: &StdPath) -> Response {
    match tokio::fs::read_to_string(playlist).await {
        Ok(contents) => playlist_response(contents),
        Err(_) => (StatusCode::NOT_FOUND, "Playlist not found").into_response(),
    }
}

fn playlist_response(contents: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
        contents,
    )
        .into_response()
}

async fn serve_segment(state: &AppState, file: &StdPath, seg: &str) -> Response {
    let mgr = &state.transcoder;
    let key = key_for(file);

    // Keep the owning session alive while its segments are being fetched.
    if let Some(session) = mgr.sessions.lock().await.get_mut(&key) {
        session.last_access = Instant::now();
    }

    let path = mgr.config.cache_dir.join(&key).join(seg);
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "video/mp2t")],
            Body::from(bytes),
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Segment not ready").into_response(),
    }
}

/// Probe the source, create the cache dir, and spawn the ffmpeg HLS process.
async fn start_session(
    config: &Config,
    probe_cache: &ProbeCache,
    file: &StdPath,
    dir: &StdPath,
) -> std::io::Result<Session> {
    let height = probe::probe(&config.ffprobe, file, probe_cache)
        .await
        .map(|p| p.height)
        .unwrap_or(0);
    let bitrate = bitrate_for(height);

    // Fresh directory so a stale/partial playlist never confuses the player.
    let _ = tokio::fs::remove_dir_all(dir).await;
    tokio::fs::create_dir_all(dir).await?;

    let log = std::fs::File::create(dir.join("ffmpeg.log"))?;

    let mut cmd = Command::new(&config.ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-i")
        .arg(file)
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("0:a:0?")
        .arg("-c:v")
        .arg(&config.encoder)
        .arg("-profile:v")
        .arg("high")
        .arg("-b:v")
        .arg(bitrate)
        .arg("-tag:v")
        .arg("avc1")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("160k")
        .arg("-ac")
        .arg("2")
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg(SEGMENT_SECONDS.to_string())
        .arg("-hls_flags")
        .arg("independent_segments")
        .arg("-hls_playlist_type")
        .arg("event")
        .arg("-hls_segment_filename")
        .arg(dir.join("seg-%05d.ts"))
        .arg(dir.join("index.m3u8"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .kill_on_drop(true);

    let child = cmd.spawn()?;
    Ok(Session {
        dir: dir.to_path_buf(),
        _child: child,
        last_access: Instant::now(),
    })
}

/// Background loop: reap idle sessions and keep the cache under `cache_max_bytes`.
pub fn spawn_sweeper(mgr: TranscodeManager) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;

            // Drop idle sessions (kill_on_drop stops their ffmpeg processes).
            {
                let mut sessions = mgr.sessions.lock().await;
                let now = Instant::now();
                sessions.retain(|_, s| now.duration_since(s.last_access) < IDLE_TIMEOUT);
            }

            evict_cache(&mgr).await;
        }
    });
}

/// Evict whole cache directories (oldest-accessed first) until under the size cap.
/// Directories belonging to live sessions are never evicted.
async fn evict_cache(mgr: &TranscodeManager) {
    let cache_dir = &mgr.config.cache_dir;
    let mut entries = match tokio::fs::read_dir(cache_dir).await {
        Ok(e) => e,
        Err(_) => return,
    };

    let active: std::collections::HashSet<PathBuf> = {
        let sessions = mgr.sessions.lock().await;
        sessions.values().map(|s| s.dir.clone()).collect()
    };

    let mut dirs: Vec<(PathBuf, u64, std::time::SystemTime)> = vec![];
    let mut total: u64 = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let size = dir_size(&path).await;
        let accessed = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.accessed().or_else(|_| m.modified()).ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        total += size;
        dirs.push((path, size, accessed));
    }

    if total <= mgr.config.cache_max_bytes {
        return;
    }

    dirs.sort_by_key(|(_, _, accessed)| *accessed); // oldest first
    for (path, size, _) in dirs {
        if total <= mgr.config.cache_max_bytes {
            break;
        }
        if active.contains(&path) {
            continue;
        }
        if tokio::fs::remove_dir_all(&path).await.is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

async fn dir_size(dir: &StdPath) -> u64 {
    let mut total = 0;
    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(meta) = entry.metadata().await
                && meta.is_file()
            {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::{bitrate_for, is_segment_name};

    #[test]
    fn segment_names_are_strictly_validated() {
        assert!(is_segment_name("seg-00000.ts"));
        assert!(is_segment_name("seg-12.ts"));
        // Anything that could escape the cache dir is rejected.
        assert!(!is_segment_name("seg-.ts"));
        assert!(!is_segment_name("seg-1a.ts"));
        assert!(!is_segment_name("../seg-1.ts"));
        assert!(!is_segment_name("seg-1.ts/../../etc/passwd"));
        assert!(!is_segment_name("index.m3u8"));
        assert!(!is_segment_name("ffmpeg.log"));
    }

    #[test]
    fn bitrate_scales_with_height() {
        assert_eq!(bitrate_for(2160), "12M");
        assert_eq!(bitrate_for(1080), "6M");
        assert_eq!(bitrate_for(720), "3M");
        assert_eq!(bitrate_for(360), "900k");
    }
}
