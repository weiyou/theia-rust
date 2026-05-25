//! Direct byte-range streaming of media files (no transcoding).
use crate::scan::{decode_path, resolve_within};
use crate::AppState;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use std::path::Path as StdPath;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Read this many bytes per loop iteration. Keeps per-stream memory flat
/// regardless of file size (the old code allocated up to 256MB per chunk).
const READ_BUF: usize = 256 * 1024;

/// Guess a content type from the file extension for direct playback.
fn content_type(path: &StdPath) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("avi") => "video/x-msvideo",
        Some("ts") => "video/mp2t",
        Some("flv") => "video/x-flv",
        _ => "application/octet-stream",
    }
}

/// Stream a media file with full HTTP byte-range support.
pub async fn stream_handler(
    Path(encoded_path): Path<String>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let decoded = match decode_path(&encoded_path) {
        Some(d) => d,
        None => return (StatusCode::BAD_REQUEST, "Invalid path encoding").into_response(),
    };

    let full_path = match resolve_within(&state.config.root, &decoded) {
        Some(p) if state.config.is_media(&p) && p.is_file() => p,
        _ => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };

    let metadata = match tokio::fs::metadata(&full_path).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read file metadata: {e}"),
            )
                .into_response();
        }
    };
    let file_size = metadata.len();

    let mut status = StatusCode::OK;
    let mut headers_resp = HeaderMap::new();
    headers_resp.insert(header::CONTENT_TYPE, content_type(&full_path).parse().unwrap());
    headers_resp.insert("Accept-Ranges", "bytes".parse().unwrap());

    let (start_byte, end_byte) = if let Some(range_header) = headers.get("range")
        && let Ok(range_str) = range_header.to_str()
        && let Some(stripped) = range_str.strip_prefix("bytes=")
    {
        let ranges: Vec<&str> = stripped.split('-').collect();
        let start = ranges[0].parse::<u64>().unwrap_or(0);
        let end = if ranges.len() > 1 && !ranges[1].is_empty() {
            ranges[1].parse::<u64>().unwrap_or(file_size - 1)
        } else {
            file_size - 1
        };

        if start >= file_size || end >= file_size || start > end {
            return (StatusCode::RANGE_NOT_SATISFIABLE, "Requested range is invalid")
                .into_response();
        }

        status = StatusCode::PARTIAL_CONTENT;
        headers_resp.insert(
            "Content-Range",
            format!("bytes {start}-{end}/{file_size}").parse().unwrap(),
        );
        headers_resp.insert(
            header::CONTENT_LENGTH,
            (end - start + 1).to_string().parse().unwrap(),
        );
        (start, end)
    } else {
        headers_resp.insert(header::CONTENT_LENGTH, file_size.to_string().parse().unwrap());
        (0, file_size - 1)
    };

    let stream = async_stream::stream! {
        let mut file = match tokio::fs::File::open(&full_path).await {
            Ok(f) => f,
            Err(e) => {
                yield Err(std::io::Error::other(format!("Failed to open file: {e}")));
                return;
            }
        };

        if start_byte > 0
            && let Err(e) = file.seek(std::io::SeekFrom::Start(start_byte)).await {
                yield Err(std::io::Error::other(format!("Failed to seek file: {e}")));
                return;
            }

        let mut remaining = (end_byte - start_byte + 1) as usize;
        let mut buffer = vec![0u8; READ_BUF];
        while remaining > 0 {
            let want = std::cmp::min(READ_BUF, remaining);
            match file.read(&mut buffer[..want]).await {
                Ok(0) => return, // EOF earlier than expected
                Ok(n) => {
                    yield Ok(axum::body::Bytes::copy_from_slice(&buffer[..n]));
                    remaining -= n;
                }
                Err(e) => {
                    yield Err(std::io::Error::other(format!("Failed to read file chunk: {e}")));
                    return;
                }
            }
        }
    };

    (status, headers_resp, Body::from_stream(stream)).into_response()
}
