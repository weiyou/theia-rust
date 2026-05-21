//! Player page that plays a file as transcoded H.264/AAC HLS.
use crate::scan::{decode_path, encode_path, resolve_within};
use crate::AppState;
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
};

/// `GET /play/{enc}` — full-screen player wired to the on-demand HLS stream.
pub async fn play_handler(
    Path(encoded_path): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let decoded = match decode_path(&encoded_path) {
        Some(d) => d,
        None => return (StatusCode::BAD_REQUEST, "Invalid path encoding").into_response(),
    };
    if resolve_within(&state.config.root, &decoded)
        .filter(|p| state.config.is_media(p) && p.is_file())
        .is_none()
    {
        return (StatusCode::NOT_FOUND, "File not found").into_response();
    }

    // Re-encode for URL-safe links (axum already percent-decoded the param).
    let enc = encode_path(&decoded);
    let name = decoded.rsplit('/').next().unwrap_or(&decoded);
    let title = html_escape::encode_text(name);
    let hls_url = format!("/hls/{enc}/index.m3u8");
    let direct_url = format!("/stream/{enc}");

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<title>{title}</title>
<style>
  html,body {{ margin:0; background:#000; height:100%; }}
  video {{ width:100vw; height:100vh; background:#000; }}
  .bar {{ position:fixed; top:0; left:0; right:0; padding:8px 12px; color:#ccc;
          font:14px system-ui,sans-serif; background:rgba(0,0,0,0.5); }}
  .bar a {{ color:#4af; text-decoration:none; margin-left:12px; }}
</style>
</head>
<body>
<div class="bar">▶ H264/AAC: {title} <a href="/">← Library</a> <a href="{direct_url}">Direct stream</a></div>
<video id="v" controls autoplay playsinline></video>
<script>
  var v = document.getElementById('v');
  var src = "{hls_url}";
  if (v.canPlayType('application/vnd.apple.mpegurl')) {{
    // Safari / iOS / iPadOS: native HLS.
    v.src = src;
  }} else {{
    // Other browsers: load hls.js on demand.
    var s = document.createElement('script');
    s.src = 'https://cdn.jsdelivr.net/npm/hls.js@1';
    s.onload = function () {{
      if (window.Hls && Hls.isSupported()) {{
        var hls = new Hls();
        hls.loadSource(src);
        hls.attachMedia(v);
      }} else {{
        v.src = src;
      }}
    }};
    document.head.appendChild(s);
  }}
</script>
</body>
</html>"#
    );

    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], html).into_response()
}
