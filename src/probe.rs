//! ffprobe-based media metadata, cached by path + mtime.
use crate::scan::{decode_path, resolve_within};
use crate::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path as StdPath;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::process::Command;

/// Codec / dimension / duration summary for a media file.
#[derive(Clone, Debug, Serialize)]
pub struct Probe {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub vcodec: String,
    pub acodec: String,
    pub size: u64,
}

/// True when the iPad can play this stream natively (H.264/HEVC video, AAC/MP3 audio).
/// Used to decide a sensible default and to hint the UI; the transcode button is shown
/// on every file regardless.
impl Probe {
    pub fn ipad_native(&self) -> bool {
        matches!(self.vcodec.as_str(), "h264" | "hevc" | "" )
            && matches!(self.acodec.as_str(), "aac" | "mp3" | "")
    }
}

pub type ProbeCache = Arc<Mutex<HashMap<PathBuf, (SystemTime, Probe)>>>;

pub fn new_cache() -> ProbeCache {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Probe a file, returning cached results when the file's mtime is unchanged.
pub async fn probe(ffprobe: &StdPath, file: &StdPath, cache: &ProbeCache) -> Option<Probe> {
    let meta = tokio::fs::metadata(file).await.ok()?;
    let mtime = meta.modified().ok()?;
    let size = meta.len();

    if let Ok(guard) = cache.lock()
        && let Some((cached_mtime, probe)) = guard.get(file)
        && *cached_mtime == mtime
    {
        return Some(probe.clone());
    }

    let output = Command::new(ffprobe)
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(file)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let probe = parse_probe(&json, size);

    if let Ok(mut guard) = cache.lock() {
        guard.insert(file.to_path_buf(), (mtime, probe.clone()));
    }
    Some(probe)
}

fn parse_probe(json: &serde_json::Value, size: u64) -> Probe {
    let empty = vec![];
    let streams = json["streams"].as_array().unwrap_or(&empty);

    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video");
    let audio = streams
        .iter()
        .find(|s| s["codec_type"] == "audio");

    let duration = json["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse::<f64>().ok())
        .or_else(|| {
            video
                .and_then(|v| v["duration"].as_str())
                .and_then(|d| d.parse::<f64>().ok())
        })
        .unwrap_or(0.0);

    Probe {
        duration,
        width: video.and_then(|v| v["width"].as_u64()).unwrap_or(0) as u32,
        height: video.and_then(|v| v["height"].as_u64()).unwrap_or(0) as u32,
        vcodec: video
            .and_then(|v| v["codec_name"].as_str())
            .unwrap_or("")
            .to_string(),
        acodec: audio
            .and_then(|a| a["codec_name"].as_str())
            .unwrap_or("")
            .to_string(),
        size,
    }
}

/// `GET /meta/{enc}` — JSON metadata used by the listing page to fill in badges lazily.
pub async fn meta_handler(
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

    match probe(&state.config.ffprobe, &file, &state.probe_cache).await {
        Some(p) => Json(p).into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "Probe failed").into_response(),
    }
}
