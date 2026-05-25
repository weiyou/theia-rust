//! On-demand HLS transcoding: VP9/AV1/Opus (or anything) -> H.264/AAC.
//!
//! Supports two output formats (controlled by `hls_segment_format` in config):
//! - "fmp4": modern fragmented MP4 (.m4s)
//! - "ts" (default): legacy MPEG-TS (.ts) — best compatibility with older devices
//!
//! On Apple Silicon (M1+), when using a `*_videotoolbox` encoder we enable
//! hardware decoding (`-hwaccel videotoolbox`) plus improved rate control
//! with headroom (`-maxrate`, dynamic `-bufsize`, `-qmin`/`-qmax`, `-realtime`).
//! See GitHub issue #8 for the full set of Apple Silicon improvements.
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

/// Segment-on-demand (#6): how many segments one on-demand ffmpeg invocation
/// produces. Requests are snapped to a fixed grid of this size so concurrent
/// requests for nearby segments share a single transcode (and so windows are
/// non-overlapping and deduplicatable). 5 × 6s = 30s per window.
const WINDOW_SEGMENTS: u32 = 5;
/// How long a segment request waits for an in-flight window transcode to
/// produce the requested segment before giving up. Generous to absorb the
/// keyframe-decode cost of a deep seek into a large file.
const WINDOW_WAIT: Duration = Duration::from_secs(25);

/// Maximum age for a completed HLS cache directory before it is eligible for
/// time-based cleanup (even if we are under the size limit).
const MAX_CACHE_AGE: Duration = Duration::from_secs(3 * 3600); // 3 hours

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
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
    /// Linear mode holds a permit in each active Session; segment-on-demand
    /// mode (#6) holds a permit only for the duration of each short window job.
    semaphore: Arc<Semaphore>,
    /// In-flight segment-on-demand window transcodes, keyed by
    /// `"{file_key}:{window_start_segment}"`. Used to deduplicate concurrent
    /// requests for segments that fall in the same window so we never spawn a
    /// duplicate ffmpeg for it. Entries are removed when the job exits.
    windows: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl TranscodeManager {
    pub fn new(config: Arc<Config>, probe_cache: ProbeCache) -> Self {
        let permits = config.max_concurrent_transcodes.max(1);
        Self {
            config: config.clone(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            probe_cache,
            semaphore: Arc::new(Semaphore::new(permits)),
            windows: Arc::new(Mutex::new(std::collections::HashSet::new())),
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

/// Scale a bitrate string (e.g. "6M", "3000k") by a factor.
/// Used for dynamic -maxrate and -bufsize calculation.
fn scale_bitrate(rate: &str, factor: f32) -> String {
    let rate = rate.trim();
    let (num_str, unit) = if let Some(s) = rate.strip_suffix('M') {
        (s, "M")
    } else if let Some(s) = rate.strip_suffix('k') {
        (s, "k")
    } else {
        (rate, "")
    };

    if let Ok(n) = num_str.parse::<f32>() {
        let scaled = (n * factor).round() as u32;
        format!("{}{}", scaled, unit)
    } else {
        rate.to_string()
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
        serve_playlist(&state, &file, &encoded_path).await
    } else if is_segment_name(&seg) {
        serve_segment(&state, &file, &seg).await
    } else {
        (StatusCode::BAD_REQUEST, "Invalid segment name").into_response()
    }
}

/// Only allow valid segment names (`seg-NNNNN.ts` or `seg-NNNNN.m4s`)
/// so the segment name can't escape the cache dir.
fn is_segment_name(seg: &str) -> bool {
    let allowed = [".ts", ".m4s"];
    for ext in allowed {
        if let Some(digits) = seg
            .strip_prefix("seg-")
            .and_then(|s| s.strip_suffix(ext))
        {
            return !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
        }
    }
    false
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

/// Recommended rate control parameters for a hardware encoder family.
#[derive(Clone, Copy, Debug)]
struct RateControl {
    maxrate_factor: f32,
    bufsize_factor: f32,
    qmin: i32,
    qmax: i32,
}

impl Default for RateControl {
    fn default() -> Self {
        Self {
            maxrate_factor: 1.15,
            bufsize_factor: 2.0,
            qmin: 15,
            qmax: 32,
        }
    }
}

/// A small profile describing the recommended flags for a given encoder.
/// This makes it easy to support new hardware backends (AMF, VAAPI, NVENC, etc.)
/// without scattering `if encoder.contains(...)` checks everywhere.
#[derive(Clone, Debug, Default)]
struct EncoderProfile {
    /// Recommended value for `-hwaccel`, if any.
    /// The caller is responsible for emitting this early (before `-i`).
    hwaccel: Option<&'static str>,
    pix_fmt: Option<&'static str>,
    realtime: bool,
    rate_control: Option<RateControl>,
}

/// Build an `EncoderProfile` for the given encoder name.
/// Rate-control factors are resolved against the target bitrate later,
/// in `apply_encoder_specific_flags`.
fn profile_for_encoder(encoder: &str) -> EncoderProfile {
    if encoder.contains("videotoolbox") {
        EncoderProfile {
            hwaccel: Some("videotoolbox"),
            pix_fmt: Some("yuv420p"),
            realtime: true,
            rate_control: Some(RateControl {
                maxrate_factor: 1.2,
                bufsize_factor: 2.4,
                qmin: 15,
                qmax: 32,
            }),
        }
    } else if encoder.contains("amf") {
        EncoderProfile {
            hwaccel: Some("d3d11va"),
            pix_fmt: Some("yuv420p"),
            realtime: false,
            rate_control: Some(RateControl {
                maxrate_factor: 1.15,
                bufsize_factor: 2.0,
                qmin: 15,
                qmax: 32,
            }),
        }
    } else if encoder.contains("vaapi") {
        EncoderProfile {
            hwaccel: Some("vaapi"),
            pix_fmt: Some("yuv420p"),
            realtime: false,
            rate_control: Some(RateControl {
                maxrate_factor: 1.15,
                bufsize_factor: 2.0,
                qmin: 15,
                qmax: 32,
            }),
        }
    } else {
        // Software or unknown encoder – no special hardware tweaks
        EncoderProfile::default()
    }
}

/// Apply an `EncoderProfile` to a ffmpeg command.
/// This is the central place for all encoder-family specific flags.
fn apply_encoder_specific_flags(cmd: &mut Command, profile: &EncoderProfile, bitrate: &str) {
    if let Some(pf) = profile.pix_fmt {
        cmd.arg("-pix_fmt").arg(pf);
    }

    if profile.realtime {
        cmd.arg("-realtime").arg("1");
    }

    if let Some(rc) = profile.rate_control {
        let maxrate = scale_bitrate(bitrate, rc.maxrate_factor);
        let bufsize = scale_bitrate(bitrate, rc.bufsize_factor);

        cmd.arg("-maxrate").arg(&maxrate);
        cmd.arg("-bufsize").arg(&bufsize);
        cmd.arg("-qmin").arg(rc.qmin.to_string());
        cmd.arg("-qmax").arg(rc.qmax.to_string());
    }
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

    // Check manifest + source mtime/size
    let manifest_ok = match load_manifest(dir).await {
        Some(m) => source_matches_manifest(file, &m).await,
        None => false,
    };

    if !manifest_ok {
        let _ = tokio::fs::remove_dir_all(dir).await;
        return false;
    }

    // Additional check: if the previous transcode never finished (no ENDLIST),
    // treat the cache as incomplete and restart.
    if let Ok(contents) = tokio::fs::read_to_string(playlist).await {
        if !contents.contains("EXT-X-ENDLIST") {
            let _ = tokio::fs::remove_dir_all(dir).await;
            return false;
        }
    } else {
        // Can't read playlist → treat as bad
        let _ = tokio::fs::remove_dir_all(dir).await;
        return false;
    }

    true
}

async fn serve_playlist(state: &AppState, file: &StdPath, encoded_path: &str) -> Response {
    let mgr = &state.transcoder;
    let config = &mgr.config;

    if config.is_segment_on_demand() {
        return serve_playlist_segment(mgr, file).await;
    }

    let key = key_for(file);
    let dir = config.cache_dir.join(&key);
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
                    // Try to surface the last part of the ffmpeg log for debugging
                    if let Ok(log_bytes) = tokio::fs::read(dir.join("ffmpeg.log")).await {
                        let tail = String::from_utf8_lossy(&log_bytes[log_bytes.len().saturating_sub(4096)..]);
                        tracing::error!(
                            file = %file.display(),
                            error = %e,
                            "Transcoder failed to start. Recent ffmpeg log tail:\n{}",
                            tail
                        );
                    }
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!(
                            "Failed to start transcoder: {e}. Check GET /hls/{}/ffmpeg.log for the full log.",
                            encoded_path
                        ),
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
            // Log the tail of the ffmpeg log for debugging on timeout
            if let Ok(log_bytes) = tokio::fs::read(dir.join("ffmpeg.log")).await {
                let tail = String::from_utf8_lossy(&log_bytes[log_bytes.len().saturating_sub(4096)..]);
                tracing::error!(
                    file = %file.display(),
                    "Transcoder timed out waiting for first segment. Recent ffmpeg log tail:\n{}",
                    tail
                );
            }
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Transcoder did not produce output in time. Check GET /hls/{}/ffmpeg.log for details.",
                    encoded_path
                ),
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

/// Generate a complete VOD-style HLS playlist (every segment listed up front,
/// terminated by `#EXT-X-ENDLIST`) from a known duration.
///
/// This is the playlist served in segment-on-demand mode (#6): the player sees
/// the whole timeline immediately and can seek anywhere; each `.ts` segment is
/// transcoded on first request (see [`serve_segment_segment`]).
pub fn generate_vod_playlist(duration: f64, segment_seconds: u32) -> String {
    if duration <= 0.0 {
        return "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-ENDLIST\n".to_string();
    }

    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
    playlist.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", segment_seconds));
    playlist.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    playlist.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");

    let mut time = 0.0;
    let mut seq = 0;

    while time < duration {
        let remaining = duration - time;
        let seg_dur = remaining.min(segment_seconds as f64);

        playlist.push_str(&format!("#EXTINF:{:.3},\n", seg_dur));
        playlist.push_str(&format!("seg-{:05}.ts\n", seq));

        time += segment_seconds as f64;
        seq += 1;
    }

    playlist.push_str("#EXT-X-ENDLIST\n");
    playlist
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

    if mgr.config.is_segment_on_demand() {
        return serve_segment_segment(mgr, file, seg).await;
    }

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

// ===========================================================================
// Segment-on-demand mode (#6)
//
// Instead of one long ffmpeg producing a growing playlist from t=0, segment
// mode serves a complete VOD playlist immediately (computed from the probed
// duration) and transcodes segments on demand. Requests are snapped to a fixed
// grid of WINDOW_SEGMENTS, so one short ffmpeg invocation — seeked to the
// window start, with forced 6s keyframes and `-output_ts_offset` so segment N's
// PTS is exactly 6*N — produces a run of segments that line up seamlessly with
// the playlist and with adjacent windows. Each window job holds a concurrency
// permit only while it runs (so #4's limit still applies), and concurrent
// requests for the same window are deduplicated.
//
// fMP4 is not yet supported in this mode; segments are always MPEG-TS.
// ===========================================================================

/// First segment index of the window containing `seg`.
fn window_start(seg: u32) -> u32 {
    (seg / WINDOW_SEGMENTS) * WINDOW_SEGMENTS
}

/// Parse the segment index out of a validated `seg-NNNNN.ts` / `.m4s` name.
fn parse_segment_number(seg: &str) -> Option<u32> {
    seg.strip_prefix("seg-")
        .and_then(|s| s.split('.').next())
        .and_then(|n| n.parse().ok())
}

/// Build the source manifest for a file from its probe + filesystem metadata.
async fn build_manifest(file: &StdPath, probe: &probe::Probe) -> CacheManifest {
    let meta = tokio::fs::metadata(file).await.ok();
    let source_mtime_unix = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    CacheManifest {
        source_mtime_unix,
        source_size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
        duration: probe.duration,
        width: probe.width,
        height: probe.height,
        vcodec: probe.vcodec.clone(),
        acodec: probe.acodec.clone(),
    }
}

/// Ensure the cache dir for a segment-mode file is present and not stale.
/// If the manifest is missing or the source changed, the directory is wiped
/// (dropping segments that no longer match) and a fresh manifest written.
async fn ensure_segment_cache_fresh(file: &StdPath, dir: &StdPath, probe: &probe::Probe) {
    let fresh = match load_manifest(dir).await {
        Some(m) => source_matches_manifest(file, &m).await,
        None => false,
    };
    if !fresh {
        let _ = tokio::fs::remove_dir_all(dir).await;
        if tokio::fs::create_dir_all(dir).await.is_ok() {
            let _ = write_manifest(dir, &build_manifest(file, probe).await).await;
        }
    }
}

/// `GET /hls/{enc}/index.m3u8` in segment-on-demand mode: serve the full VOD
/// playlist computed from the probed duration. Segments are produced on demand.
async fn serve_playlist_segment(mgr: &TranscodeManager, file: &StdPath) -> Response {
    let config = &mgr.config;
    let probe = match probe::probe(&config.ffprobe, file, &mgr.probe_cache).await {
        Some(p) if p.duration > 0.0 => p,
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not probe media duration for segment-on-demand playlist",
            )
                .into_response();
        }
    };

    let dir = config.cache_dir.join(key_for(file));
    ensure_segment_cache_fresh(file, &dir, &probe).await;

    playlist_response(generate_vod_playlist(probe.duration, SEGMENT_SECONDS))
}

/// `GET /hls/{enc}/seg-NNNNN.ts` in segment-on-demand mode.
async fn serve_segment_segment(mgr: &TranscodeManager, file: &StdPath, seg: &str) -> Response {
    let config = &mgr.config;
    let dir = config.cache_dir.join(key_for(file));

    let Some(seg_num) = parse_segment_number(seg) else {
        return (StatusCode::BAD_REQUEST, "Invalid segment name").into_response();
    };
    // Canonical on-disk name (ffmpeg writes zero-padded; the playlist requests
    // the same name), so a short request name never misses a padded file.
    let seg_path = dir.join(format!("seg-{seg_num:05}.ts"));

    // Fast path: already transcoded.
    if tokio::fs::try_exists(&seg_path).await.unwrap_or(false) {
        let resp = read_segment(&seg_path).await;
        maybe_prefetch_next_window(mgr, file, &dir, seg_num);
        return resp;
    }

    // Reject requests past the end of media (defensive; the playlist never lists
    // these, so well-behaved players won't ask).
    if let Some(m) = load_manifest(&dir).await
        && (seg_num as f64) * SEGMENT_SECONDS as f64 >= m.duration
    {
        return (StatusCode::NOT_FOUND, "Segment past end of media").into_response();
    }

    // Kick off (or join) the window transcode that will produce this segment.
    let win_start = window_start(seg_num);
    if let Err(e) = ensure_window(mgr, file, &dir, win_start).await {
        tracing::error!(file = %file.display(), "Failed to start window transcode: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to start segment transcode")
            .into_response();
    }

    // Wait for our segment to land on disk.
    let deadline = Instant::now() + WINDOW_WAIT;
    loop {
        if tokio::fs::try_exists(&seg_path).await.unwrap_or(false) {
            let resp = read_segment(&seg_path).await;
            maybe_prefetch_next_window(mgr, file, &dir, seg_num);
            return resp;
        }
        if Instant::now() >= deadline {
            log_window_failure(&dir, win_start, file).await;
            return (StatusCode::SERVICE_UNAVAILABLE, "Segment not produced in time")
                .into_response();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Read a finished segment file into a response.
async fn read_segment(path: &StdPath) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "video/mp2t")],
            Body::from(bytes),
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Segment not ready").into_response(),
    }
}

/// Start the window transcode for `win_start` unless one is already running.
/// Deduplicates via the `windows` set, acquires a concurrency permit for the
/// (short) lifetime of the job, and spawns a monitor task that releases the
/// permit and clears the in-flight marker when ffmpeg exits.
async fn ensure_window(
    mgr: &TranscodeManager,
    file: &StdPath,
    dir: &StdPath,
    win_start: u32,
) -> std::io::Result<()> {
    let wkey = format!("{}:{}", key_for(file), win_start);

    // Claim the window (or bail if someone else already owns it).
    {
        let mut windows = mgr.windows.lock().await;
        if windows.contains(&wkey) {
            return Ok(());
        }
        windows.insert(wkey.clone());
    }

    // Acquire a permit (may queue if we are at the concurrency limit). The
    // windows lock is already released, so other requests for this window see
    // the claim and just wait for the segment to appear.
    let permit = match mgr.semaphore.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            mgr.windows.lock().await.remove(&wkey);
            return Err(std::io::Error::other("transcode semaphore closed"));
        }
    };

    tokio::fs::create_dir_all(dir).await?;

    let bitrate = bitrate_for(probe_height(mgr, file).await);
    let log = std::fs::File::create(dir.join(format!("ffmpeg-win{win_start:05}.log")))?;
    let mut cmd = build_window_command(&mgr.config, file, dir, win_start, bitrate);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .kill_on_drop(true);

    match cmd.spawn() {
        Ok(mut child) => {
            let windows = mgr.windows.clone();
            tokio::spawn(async move {
                // Hold the permit until ffmpeg exits, then release it + the claim.
                let _permit = permit;
                let _ = child.wait().await;
                windows.lock().await.remove(&wkey);
            });
            Ok(())
        }
        Err(e) => {
            drop(permit);
            mgr.windows.lock().await.remove(&wkey);
            Err(e)
        }
    }
}

/// When the last segment of a window is served, eagerly start the next window so
/// continuous playback doesn't stall at the window boundary. Bounded: only the
/// immediately-following window of the file currently being watched.
fn maybe_prefetch_next_window(mgr: &TranscodeManager, file: &StdPath, dir: &StdPath, seg_num: u32) {
    if seg_num != window_start(seg_num) + WINDOW_SEGMENTS - 1 {
        return;
    }
    let next_start = seg_num + 1;
    let mgr = mgr.clone();
    let file = file.to_path_buf();
    let dir = dir.to_path_buf();
    tokio::spawn(async move {
        // Skip if the next window is past the end of media.
        if let Some(m) = load_manifest(&dir).await
            && (next_start as f64) * SEGMENT_SECONDS as f64 >= m.duration
        {
            return;
        }
        // Skip if already produced.
        if tokio::fs::try_exists(dir.join(format!("seg-{next_start:05}.ts")))
            .await
            .unwrap_or(false)
        {
            return;
        }
        let _ = ensure_window(&mgr, &file, &dir, next_start).await;
    });
}

/// Probe the source height for bitrate selection (falls back to 720).
async fn probe_height(mgr: &TranscodeManager, file: &StdPath) -> u32 {
    probe::probe(&mgr.config.ffprobe, file, &mgr.probe_cache)
        .await
        .map(|p| p.height)
        .unwrap_or(720)
}

/// Build the ffmpeg command that transcodes one window of `WINDOW_SEGMENTS`
/// segments starting at segment `win_start`.
///
/// Crucial flags (validated against ffmpeg 8.x): `-ss win_start*6` seeks to the
/// window start (resetting output PTS to 0); `-output_ts_offset win_start*6`
/// shifts the output back onto the global timeline; `-muxpreload 0 -muxdelay 0`
/// removes the MPEG-TS initial-PTS so segment N's video PTS lands exactly on
/// `6*N`; `-force_key_frames expr:gte(t,n_forced*6)` forces a keyframe at every
/// 6s boundary so segments are exactly 6s and independently decodable; and
/// `-start_number win_start` names the files `seg-{win_start..}.ts`.
fn build_window_command(
    config: &Config,
    file: &StdPath,
    dir: &StdPath,
    win_start: u32,
    bitrate: &str,
) -> Command {
    let offset = (win_start * SEGMENT_SECONDS).to_string();
    let window_secs = (WINDOW_SEGMENTS * SEGMENT_SECONDS).to_string();

    let mut cmd = Command::new(&config.ffmpeg);
    cmd.arg("-hide_banner").arg("-loglevel").arg("warning");

    let profile = profile_for_encoder(&config.encoder);
    if let Some(hw) = profile.hwaccel {
        cmd.arg("-hwaccel").arg(hw);
    }

    cmd.arg("-ss")
        .arg(&offset)
        .arg("-i")
        .arg(file)
        .arg("-t")
        .arg(&window_secs)
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("0:a:0?")
        .arg("-c:v")
        .arg(&config.encoder)
        .arg("-profile:v")
        .arg("high")
        .arg("-b:v")
        .arg(bitrate);

    apply_encoder_specific_flags(&mut cmd, &profile, bitrate);

    cmd.arg("-force_key_frames")
        .arg(format!("expr:gte(t,n_forced*{SEGMENT_SECONDS})"))
        .arg("-tag:v")
        .arg("avc1")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("160k")
        .arg("-ac")
        .arg("2")
        // Remove the MPEG-TS initial PTS so seg N's PTS == 6*N exactly.
        .arg("-muxpreload")
        .arg("0")
        .arg("-muxdelay")
        .arg("0")
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg(SEGMENT_SECONDS.to_string())
        .arg("-hls_flags")
        .arg("independent_segments+temp_file")
        .arg("-hls_playlist_type")
        .arg("vod")
        .arg("-hls_list_size")
        .arg("0")
        .arg("-start_number")
        .arg(win_start.to_string())
        .arg("-output_ts_offset")
        .arg(&offset)
        .arg("-hls_segment_filename")
        .arg(dir.join("seg-%05d.ts"))
        // ffmpeg's own per-window playlist; we serve our generated VOD playlist
        // instead, so this is a throwaway it needs as the output target.
        .arg(dir.join(format!(".win{win_start:05}.m3u8")));

    cmd
}

/// On a window-transcode timeout, surface the tail of its ffmpeg log.
async fn log_window_failure(dir: &StdPath, win_start: u32, file: &StdPath) {
    if let Ok(bytes) = tokio::fs::read(dir.join(format!("ffmpeg-win{win_start:05}.log"))).await {
        let tail = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(4096)..]);
        tracing::error!(
            file = %file.display(),
            window = win_start,
            "Segment-on-demand window transcode timed out. ffmpeg log tail:\n{}",
            tail
        );
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

    let use_fmp4 = config.uses_fmp4_segments();

    let segment_ext = if use_fmp4 { "m4s" } else { "ts" };
    let segment_pattern = format!("seg-%05d.{}", segment_ext);
    // For -hls_fmp4_init_filename, ffmpeg expects a filename (relative to the playlist),
    // not a full absolute path. Using a full path here can cause "Failed to open segment" errors.
    let init_filename: String = if use_fmp4 {
        "init.mp4".to_string()
    } else {
        String::new()
    };

    let mut cmd = Command::new(&config.ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning");

    // Encoder-family profile (videotoolbox / amf / vaapi / software). Computed once
    // and reused below; the hwaccel flag must be emitted *before* -i.
    let profile = profile_for_encoder(&config.encoder);
    if let Some(hw) = profile.hwaccel {
        cmd.arg("-hwaccel").arg(hw);
    }

    cmd.arg("-i")
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
        .arg(bitrate);

    // Apply encoder-family specific flags (rate control, pix_fmt, realtime, etc.)
    apply_encoder_specific_flags(&mut cmd, &profile, bitrate);

    cmd.arg("-tag:v")
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
        .arg(SEGMENT_SECONDS.to_string());

    // === HLS output format (legacy TS vs modern fMP4) ===
    if use_fmp4 {
        cmd.arg("-hls_segment_type")
            .arg("fmp4")
            .arg("-hls_fmp4_init_filename")
            .arg(&init_filename)
            .arg("-hls_list_size")
            .arg("0");
    } else {
        cmd.arg("-hls_playlist_type")
            .arg("event");
    }

    // Common flags + delete_segments (prevents long-running sessions from filling disk)
    let mut hls_flags = String::from("independent_segments");
    if use_fmp4 {
        hls_flags.push_str("+delete_segments");
    }
    cmd.arg("-hls_flags")
        .arg(hls_flags)
        .arg("-hls_segment_filename")
        .arg(dir.join(&segment_pattern))
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
    let now = std::time::SystemTime::now();

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

        // Time-based cleanup: delete anything older than MAX_CACHE_AGE (3 hours)
        // even if we are still under the size limit.
        if let Ok(age) = now.duration_since(accessed)
            && age > MAX_CACHE_AGE
            && !active.contains(&path)
            && tokio::fs::remove_dir_all(&path).await.is_ok()
        {
            // do not add to total / dirs
            continue;
        }

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
    /// In-flight segment-on-demand window transcodes (#6). Always 0 in linear mode.
    pub active_segment_windows: usize,
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

    let active_segment_windows = mgr.windows.lock().await.len();

    let status = TranscodeStatus {
        active_transcodes: sessions.len(),
        max_concurrent: mgr.config.max_concurrent_transcodes,
        active_segment_windows,
        sessions: infos,
    };

    (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], serde_json::to_string(&status).unwrap())
        .into_response()
}

/// `GET /hls/{enc}/ffmpeg.log` — serves the ffmpeg log for a given media file (if it exists).
/// Useful for debugging transcode failures (see issue #5).
pub async fn ffmpeg_log_handler(
    Path(encoded_path): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let decoded = match decode_path(&encoded_path) {
        Some(d) => d,
        None => return (StatusCode::BAD_REQUEST, "Invalid path encoding").into_response(),
    };

    let file = match resolve_within(&state.config.root, &decoded) {
        Some(p) if state.config.is_media(&p) && p.is_file() => p,
        _ => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };

    let key = key_for(&file);
    let log_path = state.config.cache_dir.join(&key).join("ffmpeg.log");

    match tokio::fs::read(&log_path).await {
        Ok(bytes) => {
            // Cap very large logs (serve last 256KB)
            let max_size: usize = 256 * 1024;
            let content = if bytes.len() > max_size {
                bytes[bytes.len() - max_size..].to_vec()
            } else {
                bytes
            };
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                content,
            )
                .into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "No log available for this file").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bitrate_for, build_window_command, generate_vod_playlist, is_segment_name, load_manifest,
        parse_segment_number, window_start, write_manifest, CacheManifest, validate_cached_transcode,
    };
    use std::time::UNIX_EPOCH;

    #[test]
    fn window_start_snaps_to_grid() {
        // WINDOW_SEGMENTS = 5
        assert_eq!(window_start(0), 0);
        assert_eq!(window_start(4), 0);
        assert_eq!(window_start(5), 5);
        assert_eq!(window_start(9), 5);
        assert_eq!(window_start(12), 10);
        assert_eq!(parse_segment_number("seg-00012.ts"), Some(12));
        assert_eq!(parse_segment_number("seg-00012.m4s"), Some(12));
        assert_eq!(parse_segment_number("seg-7.ts"), Some(7));
        assert_eq!(parse_segment_number("index.m3u8"), None);
    }

    #[test]
    fn window_command_has_alignment_flags() {
        let config = crate::config::Config::test_default(std::path::PathBuf::from("/tmp/root"));
        // window starting at segment 5 -> offset 30s; 5 segments * 6s -> -t 30
        let cmd = build_window_command(
            &config,
            std::path::Path::new("/tmp/movie.mkv"),
            std::path::Path::new("/tmp/cache/abc"),
            5,
            "3M",
        );
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let after = |flag: &str| -> Option<String> {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1).cloned())
        };

        assert_eq!(after("-ss").as_deref(), Some("30"));
        assert_eq!(after("-output_ts_offset").as_deref(), Some("30"));
        assert_eq!(after("-start_number").as_deref(), Some("5"));
        assert_eq!(after("-t").as_deref(), Some("30"));
        assert_eq!(after("-muxpreload").as_deref(), Some("0"));
        assert_eq!(after("-muxdelay").as_deref(), Some("0"));
        assert_eq!(
            after("-force_key_frames").as_deref(),
            Some("expr:gte(t,n_forced*6)")
        );
        assert_eq!(
            after("-hls_flags").as_deref(),
            Some("independent_segments+temp_file")
        );
        // test_default uses h264_videotoolbox -> hardware decode flag present.
        assert!(args.iter().any(|a| a == "-hwaccel"));
    }

    /// End-to-end check that a windowed transcode produces correctly-named
    /// segments whose timestamps land on the global timeline (seg N -> ~6*N).
    /// Requires ffmpeg + ffprobe on PATH; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires ffmpeg/ffprobe on PATH"]
    async fn window_transcode_produces_timeline_aligned_segments() {
        use crate::{config::Config, probe};
        use std::sync::Arc;

        let tmp = std::env::temp_dir().join(format!("theia_seg_it_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let source = tmp.join("source.mp4");

        // 60s synthetic source (libx264 so the test is portable across platforms).
        let gen_status = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30",
                "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
                "-t", "60", "-c:v", "libx264", "-preset", "ultrafast",
                "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest",
            ])
            .arg(&source)
            .status()
            .expect("run ffmpeg");
        assert!(gen_status.success(), "failed to generate synthetic source");

        let mut config = Config::test_default(tmp.clone());
        config.encoder = "libx264".to_string();
        config.cache_dir = tmp.join("cache");
        config.transcode_mode = "segment".to_string();
        let config = Arc::new(config);
        let mgr = super::TranscodeManager::new(config.clone(), probe::new_cache());

        let dir = config.cache_dir.join(super::key_for(&source));
        // Mid-stream window starting at segment 5 (t = 30s).
        super::ensure_window(&mgr, &source, &dir, 5).await.unwrap();

        let seg = dir.join("seg-00005.ts");
        let mut produced = false;
        for _ in 0..200 {
            if tokio::fs::try_exists(&seg).await.unwrap_or(false) {
                produced = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(produced, "seg-00005.ts was not produced");

        // seg-5's first video PTS must be ~30.0 (global timeline alignment).
        let out = std::process::Command::new("ffprobe")
            .args([
                "-hide_banner", "-v", "error", "-select_streams", "v",
                "-show_entries", "packet=pts_time", "-of", "csv=p=0",
            ])
            .arg(&seg)
            .output()
            .expect("run ffprobe");
        let pts: f64 = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches(',')
            .parse()
            .unwrap_or(-1.0);
        assert!(
            (pts - 30.0).abs() < 0.2,
            "seg-5 first PTS should be ~30.0, got {pts}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn segment_names_are_strictly_validated() {
        assert!(is_segment_name("seg-00000.ts"));
        assert!(is_segment_name("seg-12.ts"));
        assert!(is_segment_name("seg-00001.m4s")); // fMP4 support (#8)
        // Anything that could escape the cache dir is rejected.
        assert!(!is_segment_name("seg-.ts"));
        assert!(!is_segment_name("seg-1a.ts"));
        assert!(!is_segment_name("../seg-1.ts"));
        assert!(!is_segment_name("seg-1.ts/../../etc/passwd"));
        assert!(!is_segment_name("index.m3u8"));
        assert!(!is_segment_name("ffmpeg.log"));
    }

    #[test]
    fn vod_playlist_covers_full_duration() {
        // 13s at 6s segments => segments [0,6), [6,12), [12,13): 3 entries,
        // the last one only 1s long, terminated by ENDLIST.
        let pl = generate_vod_playlist(13.0, 6);
        assert_eq!(pl.matches("#EXTINF:").count(), 3);
        assert!(pl.contains("seg-00000.ts"));
        assert!(pl.contains("seg-00002.ts"));
        assert!(!pl.contains("seg-00003.ts"));
        assert!(pl.contains("#EXTINF:1.000,"), "final partial segment should be 1s");
        assert!(pl.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(pl.trim_end().ends_with("#EXT-X-ENDLIST"));

        // A zero/unknown duration yields a valid but empty (terminated) playlist.
        let empty = generate_vod_playlist(0.0, 6);
        assert!(empty.contains("#EXT-X-ENDLIST"));
        assert_eq!(empty.matches("#EXTINF:").count(), 0);
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

        // validate should succeed while source is unchanged and playlist is complete
        let playlist = tmp.join("index.m3u8");
        tokio::fs::write(&playlist, "#EXTM3U\nseg-00000.ts\n#EXT-X-ENDLIST\n").await.unwrap();
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
