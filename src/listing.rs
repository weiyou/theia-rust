//! Directory listing HTML and the (direct-stream) "Play All" playlist page.
use crate::scan::{decode_path, encode_path, resolve_within, scan_media, MediaNode};
use crate::AppState;
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
};
use walkdir::WalkDir;

/// Human-readable byte size.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Build the HTML for one node. Filenames are HTML-escaped; encoded paths are URL-safe.
fn build_html(node: &MediaNode) -> String {
    match node {
        MediaNode::File { name, enc, size } => {
            let safe = html_escape::encode_text(name);
            let size = human_size(*size);
            format!(
                concat!(
                    r#"<li class="item"><span class="name">{safe}</span>"#,
                    r#"<span class="size">{size}</span>"#,
                    r#"<span class="badge" data-meta="/meta/{enc}"></span>"#,
                    r#"<a class="play" href="/play/{enc}">▶ H264/AAC</a>"#,
                    r#"<a class="direct" href="/stream/{enc}">stream</a></li>"#,
                ),
                safe = safe,
                size = size,
                enc = enc,
            )
        }
        MediaNode::Folder {
            name,
            enc,
            children,
        } => {
            let safe = html_escape::encode_text(name);
            let play_all = if !enc.is_empty() {
                format!(r#" <a class="playall" href="/playall/{enc}">▶ Play All</a>"#)
            } else {
                String::new()
            };
            let mut html = format!(r#"<li class="folder"><span class="name">{safe}</span>{play_all}</li>"#);
            if !children.is_empty() {
                html.push_str("<li><ul>");
                for child in children {
                    html.push_str(&build_html(child));
                }
                html.push_str("</ul></li>");
            }
            html
        }
    }
}

const STYLE: &str = r#"
  body { font-family: system-ui, -apple-system, sans-serif; max-width: 900px; margin: 1rem auto; padding: 0 1rem; }
  ul { list-style: none; padding-left: 1rem; }
  li { margin: 6px 0; }
  .item { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
  .name { flex: 1; min-width: 200px; word-break: break-word; }
  .folder .name { font-weight: 600; }
  .size { color: #888; font-size: 0.85em; }
  .badge { color: #2a7; font-size: 0.8em; font-variant: small-caps; }
  .play { background: #06c; color: #fff; padding: 3px 10px; border-radius: 6px; text-decoration: none; font-size: 0.85em; }
  .direct, .playall { color: #06c; text-decoration: none; font-size: 0.85em; }
"#;

/// Lazily fetches /meta for each file and fills in a "codec · WxH" badge.
const META_SCRIPT: &str = r#"
<script>
document.querySelectorAll('[data-meta]').forEach(function (el) {
  fetch(el.dataset.meta).then(function (r) { return r.ok ? r.json() : null; }).then(function (m) {
    if (!m) return;
    var parts = [];
    if (m.vcodec) parts.push(m.vcodec);
    if (m.width && m.height) parts.push(m.width + '×' + m.height);
    if (m.acodec) parts.push(m.acodec);
    el.textContent = parts.join(' · ');
  }).catch(function () {});
});
</script>"#;

/// `GET /` — full directory tree.
pub async fn directory_handler(State(state): State<AppState>) -> impl IntoResponse {
    let tree = scan_media(&state.config.root, "", &state.config);
    let list_html = if let MediaNode::Folder { children, .. } = &tree {
        if children.is_empty() {
            format!(
                "<p>No media found. Add files to {}.</p>",
                html_escape::encode_text(&state.config.root.display().to_string())
            )
        } else {
            let mut html = String::from("<ul>");
            for child in children {
                html.push_str(&build_html(child));
            }
            html.push_str("</ul>");
            html
        }
    } else {
        String::new()
    };

    let html = format!(
        r#"<!DOCTYPE html><html><head><meta charset="UTF-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Theia Media Library</title><style>{STYLE}</style></head><body><h1>Theia</h1>{list_html}{META_SCRIPT}</body></html>"#
    );
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], html).into_response()
}

/// `GET /playall/{enc}` — sequential direct-stream playlist for a folder.
/// (Direct stream; use the per-file H264/AAC button for incompatible codecs.)
pub async fn playall_handler(
    Path(encoded_path): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let decoded = match decode_path(&encoded_path) {
        Some(d) => d,
        None => return (StatusCode::BAD_REQUEST, "Invalid path").into_response(),
    };
    let folder_path = match resolve_within(&state.config.root, &decoded) {
        Some(p) if p.is_dir() => p,
        _ => return (StatusCode::NOT_FOUND, "Folder not found").into_response(),
    };

    let mut mp4s = vec![];
    for entry in WalkDir::new(&folder_path)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.path() == folder_path {
            continue;
        }
        if entry.path().is_file() && state.config.is_media(entry.path()) {
            let file_name = entry.file_name().to_string_lossy().to_string();
            mp4s.push(format!("{decoded}/{file_name}"));
        }
    }
    mp4s.sort_by_key(|p| p.rsplit('/').next().unwrap_or("").to_lowercase());

    let js_array = mp4s
        .iter()
        .map(|rel| format!("\"/stream/{}\"", encode_path(rel)))
        .collect::<Vec<_>>()
        .join(", ");

    let folder_name = html_escape::encode_text(decoded.rsplit('/').next().unwrap_or("Playlist"));

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Playlist - {folder_name}</title>
<style>body {{background:#000;color:#fff;font-family:system-ui}} video {{width:100%;max-height:90vh}}</style></head>
<body>
<h2>Playing: {folder_name}</h2>
<video id="player" controls autoplay playsinline></video>
<script>
const videos = [{js_array}];
let current = 0;
const player = document.getElementById('player');
if (videos.length > 0) {{
    player.src = videos[0];
    player.onended = () => {{ current++; if (current < videos.length) {{ player.src = videos[current]; }} }};
}}
</script>
</body>
</html>"#
    );
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], html).into_response()
}
