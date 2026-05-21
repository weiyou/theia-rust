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
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Semaphore, OwnedSemaphorePermit};

const SEGMENT_SECONDS: u32 = 6;
/// Kill a transcode session that has had no requests for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long to wait for ffmpeg to produce the first playlist entry.
const PLAYLIST_WAIT: Duration = Duration::from_secs(20);

struct Session {
    dir: PathBuf,
    /// Human-friendly name for status/debug pages (e.g. "movie.mkv").
    display_name: String,
    /// Held so the process is killed on drop (`kill_on_drop`).
    _child: Child,
    last_access: Instant,
    /// Keeps the concurrency slot reserved for the lifetime of this transcode.
    /// Dropped when the session is reaped or the server shuts down.
    _permit: Option<OwnedSemaphorePermit>,
}

/// Lightweight manifest stored alongside each cached HLS directory.
/// Used for stale-cache detection (P0) and as a foundation for future
/// smarter features (segment-on-demand, better eviction, etc.).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheManifest {
    /// Unix timestamp seconds of the source file's mtime at transcode time.
    source_mtime_unix: u64,
    source_size: u64,
    duration: f64,
    width: u32,
    height: u32,
    vcodec: String,
    acodec: String,
}

#[derive(Clone)]
pub struct TranscodeManager {
    config: Arc<Config>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    probe_cache: ProbeCache,
    /// Limits how many ffmpeg transcodes may run concurrently.
    /// Uses OwnedSemaphorePermit stored in each active Session.
    semaphore: Arc<Semaphore>,
}

impl TranscodeManager {
    pub fn new(config: Arc<Config>, probe_cache: ProbeCache) -> Self {
        let permits = config.max_concurrent_transcodes.max(1);
        Self {
            config: config.clone(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            probe_cache,
            semaphore: Arc::new(Semaphore::new(permits)),
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

/// Load a cache manifest if present (returns None for old caches without one).
async fn load_manifest(dir: &StdPath) -> Option<CacheManifest> {
    let path = dir.join("manifest.json");
    let bytes = tokio::fs::read(&path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write (or overwrite) the manifest for a cache directory.
async fn write_manifest(dir: &StdPath, m: &CacheManifest) -> std::io::Result<()> {
    let path = dir.join("manifest.json");
    let data = serde_json::to_vec_pretty(m)?;
    tokio::fs::write(path, data).await
}

/// Return true if the live source file still matches the manifest we captured
/// when we originally transcoded it.
async fn source_matches_manifest(file: &StdPath, m: &CacheManifest) -> bool {
    let meta = match tokio::fs::metadata(file).await {
        Ok(m) => m,
        Err(_) => return false,
    };
    let mtime = match meta.modified() {
        Ok(t) => t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        Err(_) => return false,
    };
    mtime == m.source_mtime_unix && meta.len() == m.source_size
}

/// If a completed cache exists for this file, validate that the source has not
/// changed since we built the transcode. Returns `true` if we can safely serve
/// the cached playlist. On mismatch (or missing/invalid manifest) the directory
/// is removed so the caller will fall through to starting a fresh session.
async fn validate_cached_transcode(
    file: &StdPath,
    dir: &StdPath,
    playlist: &StdPath,
) -> bool {
    if !playlist.exists() {
        return false;
    }
    match load_manifest(dir).await {
        Some(m) if source_matches_manifest(file, &m).await => true,
        _ => {
            // Stale, missing manifest (old cache), or source changed → blow it away.
            let _ = tokio::fs::remove_dir_all(dir).await;
            false
        }
    }
}

async fn serve_playlist(state: &AppState, file: &StdPath) -> Response {
    let mgr = &state.transcoder;
    let key = key_for(file);
    let dir = mgr.config.cache_dir.join(&key);
    let playlist = dir.join("index.m3u8");

    // Fast path: an existing session (or a validated completed cache) already has the playlist.
    {
        let mut sessions = mgr.sessions.lock().await;
        if let Some(session) = sessions.get_mut(&key) {
            session.last_access = Instant::now();
        } else if validate_cached_transcode(file, &dir, &playlist).await {
            // Completed transcode from a previous run and the source file has not changed.
            return read_playlist(&playlist).await;
        } else {
            // Either no cache, or we just invalidated a stale one. Start fresh
            // while still holding the lock to avoid double-spawn.
            match start_session(mgr, file, &dir).await {
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

/// Probe the source, create the cache dir, write a source manifest, acquire a
/// concurrency permit, and spawn the ffmpeg HLS process.
async fn start_session(
    mgr: &TranscodeManager,
    file: &StdPath,
    dir: &StdPath,
) -> std::io::Result<Session> {
    let config = &mgr.config;
    let probe_cache = &mgr.probe_cache;

    // Acquire a concurrency slot *before* doing heavy work (mkdir, probe, spawn).
    // This may queue if we are already at the configured limit.
    // The permit is stored in the Session and released on drop / reaping.
    let permit = mgr.semaphore.clone().acquire_owned().await
        .map_err(|_| std::io::Error::other("transcode semaphore closed"))?;

    // Capture full probe data (for manifest) + height for bitrate selection.
    let probe_data = probe::probe(&config.ffprobe, file, probe_cache).await;
    let height = probe_data.as_ref().map(|p| p.height).unwrap_or(0);
    let bitrate = bitrate_for(height);

    // Source metadata for the manifest (mtime + size).
    let source_meta = tokio::fs::metadata(file).await.ok();
    let source_mtime_unix = source_meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let source_size = source_meta.as_ref().map(|m| m.len()).unwrap_or(0);

    // Fresh directory so a stale/partial playlist never confuses the player.
    let _ = tokio::fs::remove_dir_all(dir).await;
    tokio::fs::create_dir_all(dir).await?;

    // Write the manifest *before* starting ffmpeg so that even a partial
    // failure leaves a traceable record for future invalidation logic.
    if let Some(p) = &probe_data {
        let manifest = CacheManifest {
            source_mtime_unix,
            source_size,
            duration: p.duration,
            width: p.width,
            height: p.height,
            vcodec: p.vcodec.clone(),
            acodec: p.acodec.clone(),
        };
        // Best-effort; failure to write the manifest is non-fatal for now
        // (old behavior is preserved for caches without a manifest).
        let _ = write_manifest(dir, &manifest).await;
    }

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
    let display_name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());

    Ok(Session {
        dir: dir.to_path_buf(),
        display_name,
        _child: child,
        last_access: Instant::now(),
        _permit: Some(permit),
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

/// Lightweight status for the debug endpoint.
#[derive(serde::Serialize)]
pub struct TranscodeStatus {
    pub active_transcodes: usize,
    pub max_concurrent: usize,
    pub sessions: Vec<SessionInfo>,
}

#[derive(serde::Serialize)]
pub struct SessionInfo {
    pub display_name: String,
    pub last_access_secs_ago: u64,
}

/// `GET /status` — simple JSON view of current transcoding activity (behind auth).
pub async fn status_handler(State(state): State<AppState>) -> impl IntoResponse {
    let mgr = &state.transcoder;
    let sessions = mgr.sessions.lock().await;
    let now = Instant::now();

    let mut infos: Vec<SessionInfo> = sessions
        .values()
        .map(|s| SessionInfo {
            display_name: s.display_name.clone(),
            last_access_secs_ago: now.duration_since(s.last_access).as_secs(),
        })
        .collect();

    // Sort by most recently accessed first
    infos.sort_by_key(|i| i.last_access_secs_ago);

    let status = TranscodeStatus {
        active_transcodes: sessions.len(),
        max_concurrent: mgr.config.max_concurrent_transcodes,
        sessions: infos,
    };

    (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], serde_json::to_string(&status).unwrap())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{bitrate_for, is_segment_name, load_manifest, write_manifest, CacheManifest, validate_cached_transcode};
    use std::time::UNIX_EPOCH;

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

    #[tokio::test]
    async fn manifest_roundtrips_and_detects_stale_source() {
        let tmp = std::env::temp_dir().join(format!("theia_manifest_test_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        let src_file = tmp.join("source.mp4");
        tokio::fs::write(&src_file, b"initial-content").await.unwrap();

        // Simulate what start_session does.
        let mtime = tokio::fs::metadata(&src_file)
            .await
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let manifest = CacheManifest {
            source_mtime_unix: mtime,
            source_size: 15,
            duration: 12.3,
            width: 640,
            height: 480,
            vcodec: "vp9".into(),
            acodec: "opus".into(),
        };

        write_manifest(&tmp, &manifest).await.unwrap();
        let loaded = load_manifest(&tmp).await.unwrap();
        assert_eq!(loaded.source_size, 15);
        assert_eq!(loaded.vcodec, "vp9");

        // validate should succeed while source is unchanged
        let playlist = tmp.join("index.m3u8");
        tokio::fs::write(&playlist, "#EXTM3U\nseg-00000.ts\n").await.unwrap();
        assert!(validate_cached_transcode(&src_file, &tmp, &playlist).await);

        // Now mutate the source (size changes → must be treated as stale).
        // We rely on size mismatch (the mtime check is a defense-in-depth).
        tokio::fs::write(&src_file, b"this-is-a-much-longer-replacement-that-changes-size-and-mtime").await.unwrap();

        // After mutation, the cache should be considered stale and the dir removed.
        let still_valid = validate_cached_transcode(&src_file, &tmp, &playlist).await;
        assert!(!still_valid, "stale source must cause invalidation");
        assert!(!tmp.exists() || !playlist.exists(), "stale cache dir should have been removed");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// Integration-style test for the concurrency limiter (P1).
    /// We exercise the real Semaphore + OwnedSemaphorePermit behavior used by
    /// the HLS handlers without needing a full HTTP stack or real media files.
    #[tokio::test]
    async fn concurrency_limit_is_enforced() {
        use std::sync::Arc;
        use tokio::sync::Semaphore;

        // Simulate what TranscodeManager does with a limit of 2
        let sem = Arc::new(Semaphore::new(2));

        // Acquire first two permits (simulating two active transcodes)
        let p1 = sem.clone().acquire_owned().await.unwrap();
        let p2 = sem.clone().acquire_owned().await.unwrap();

        // Third acquire should not complete immediately (we use try_acquire to prove it)
        assert!(sem.try_acquire().is_err(), "should be at limit of 2");

        // Release one (simulating a session being reaped / idle timeout)
        drop(p1);

        // Now the third should succeed
        let p3 = sem.clone().acquire_owned().await.unwrap();
        assert!(sem.try_acquire().is_err(), "still at limit after releasing one");

        // Clean up
        drop(p2);
        drop(p3);

        // After all released, we should be able to acquire again
        let _p4 = sem.try_acquire().expect("should have capacity again");
    }
}
