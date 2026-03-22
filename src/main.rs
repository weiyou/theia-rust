/// Theia Rust - Portable MP4 streaming server with playlist mode
/// Streams any size video without 2GB limits using 256MB chunked streaming
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Request, StatusCode, header},
    middleware::Next,
    response::IntoResponse,
};
use base64::Engine;
use clap::Parser;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use walkdir::WalkDir;

/// Application state holding the root media directory
#[derive(Clone)]
struct AppState {
    root_dir: PathBuf,
}

/// Tree node representing media files and folders
#[derive(Debug)]
enum MediaNode {
    File {
        name: String,
        path: String,
    },
    Folder {
        name: String,
        path: String,
        children: Vec<MediaNode>,
    },
}

/// Encode path components with %2F separators for URL-safe paths
fn encode_path(path: &str) -> String {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|comp| urlencoding::encode(comp))
        .collect::<Vec<_>>()
        .join("%2F")
}

/// Decode %2F-separated path back to normal filesystem path
fn decode_path(encoded: &str) -> Option<String> {
    urlencoding::decode(encoded).ok().map(|s| s.to_string())
}

/// Recursively scan media directory, building tree of MP4 files and folders
fn scan_media(root: &PathBuf, prefix: &str) -> MediaNode {
    let mut children = vec![];
    for entry in WalkDir::new(root)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.path() == root {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.path().is_dir() {
            let child_prefix = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            let child = scan_media(&entry.path().to_path_buf(), &child_prefix);
            children.push(child);
        } else if entry.path().extension().is_some_and(|ext| ext == "mp4") {
            let rel_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            let encoded = encode_path(&rel_path);
            children.push(MediaNode::File {
                name,
                path: format!("/stream/{}", encoded),
            });
        }
    }
    // Sort children alphanumerically by name
    children.sort_by_key(|node| match node {
        MediaNode::File { name, .. } => name.clone(),
        MediaNode::Folder { name, .. } => name.clone(),
    });
    let name = root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let path = if prefix.is_empty() {
        "".to_string()
    } else {
        encode_path(prefix)
    };
    MediaNode::Folder {
        name,
        path,
        children,
    }
}

/// Build HTML list item for a media node (file or folder)
fn build_html(node: &MediaNode) -> String {
    match node {
        MediaNode::File { name, path } => format!(
            r#"<li class="item"><div class="name"><a href="{}">{}</a></div></li>"#,
            path, name
        ),
        MediaNode::Folder {
            name,
            path,
            children,
        } => {
            let play_all = if !path.is_empty() {
                format!(
                    r#" <a href="/playall/{}" class="playall" style="margin-left:10px; color:#0066ff;">▶ Play All</a>"#,
                    path
                )
            } else {
                "".to_string()
            };
            let mut html = format!(
                r#"<li class="item"><div class="name">{}{}</div></li>"#,
                name, play_all
            );
            if !children.is_empty() {
                html += r#"<li><ul>"#;
                for child in children {
                    html += &build_html(child);
                }
                html += r#"</ul></li>"#;
            }
            html
        }
    }
}

/// Generate complete HTML page listing all media files and folders
fn generate_html_listing(root: &PathBuf) -> String {
    let tree = scan_media(root, "");
    let css = r#"body { font-family: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif, "Apple Color Emoji", "Segoe UI Emoji", "Segoe UI Symbol"; } ul { list-style: none; padding-left: 0; } li { margin-bottom: 10px; } .item { display: flex; align-items: center; } .name { flex: 1; word-break: break-word; }"#;
    let list_html = if let MediaNode::Folder { children, .. } = &tree {
        if children.is_empty() {
            r#"<p>No media found. Add files to ~/Theia_Home.</p>"#.to_string()
        } else {
            let mut html = r#"<ul>"#.to_string();
            for child in children {
                html += &build_html(child);
            }
            html += r#"</ul>"#;
            html
        }
    } else {
        "".to_string()
    };
    format!(
        r#"<!DOCTYPE html><html><head><meta charset="UTF-8"><title>Theia Media Library</title><style>{}</style></head><body>{}</body></html>"#,
        css, list_html
    )
}

/// Basic auth middleware - exempts /login, requires theia:theia for others
async fn basic_auth_middleware(
    req: Request<Body>,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    // Exempt /login
    if req.uri().path() == "/login" {
        return Ok(next.run(req).await);
    }

    let auth_header = req.headers().get("authorization");
    if let Some(auth) = auth_header
        && let Ok(auth_str) = auth.to_str()
        && let Some(base64_part) = auth_str.strip_prefix("Basic ")
        && let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(base64_part)
        && let Ok(credentials) = String::from_utf8(decoded)
        && credentials == "theia:theia"
    {
        return Ok(next.run(req).await);
    }

    // Return 401 with WWW-Authenticate header
    let response = (
        StatusCode::UNAUTHORIZED,
        [("WWW-Authenticate", "Basic realm=\"Theia\"")],
    )
        .into_response();
    Ok(response)
}

/// Login page with instructions
async fn login_handler() -> impl IntoResponse {
    let html = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>Theia</title>
</head>
<body>
<h1>Theia</h1>
<p>To access the library, visit <a href="/">/</a>. When prompted, use username: theia, password: theia.</p>
</body>
</html>"#;
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], html).into_response()
}

/// Root directory listing page
async fn directory_handler(State(state): State<AppState>) -> impl IntoResponse {
    let html = generate_html_listing(&state.root_dir);
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], html).into_response()
}

/// Playlist page for folder - auto-plays all MP4s in sequence
async fn playall_handler(
    Path(encoded_path): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Decode folder path
    let decoded = match decode_path(&encoded_path) {
        Some(d) => d,
        None => return (StatusCode::BAD_REQUEST, "Invalid path").into_response(),
    };

    // Resolve folder path
    let folder_path = state.root_dir.join(&decoded);
    if !folder_path.exists() || !folder_path.is_dir() {
        return (StatusCode::NOT_FOUND, "Folder not found").into_response();
    }

    // Scan direct MP4 children
    let mut mp4s = vec![];
    for entry in WalkDir::new(&folder_path)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.path() == folder_path {
            continue;
        }
        if entry.path().is_file() && entry.path().extension().is_some_and(|ext| ext == "mp4") {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let rel_path = format!("{}/{}", decoded, file_name);
            mp4s.push(rel_path);
        }
    }

    // Sort alphanumerically by filename
    mp4s.sort_by_key(|p| p.split('/').next_back().unwrap_or("").to_string());

    // Encode paths and build URLs
    let videos: Vec<String> = mp4s
        .into_iter()
        .map(|rel_path| {
            let encoded = encode_path(&rel_path);
            format!("/stream/{}", encoded)
        })
        .collect();

    // Folder name
    let folder_name = decoded
        .split('/')
        .next_back()
        .unwrap_or("Playlist")
        .to_string();

    // Build JS array for client-side playlist
    let js_array = videos
        .iter()
        .map(|url| format!("\"{}\"", url))
        .collect::<Vec<_>>()
        .join(", ");

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
<title>Playlist - {}</title>
<style>body {{background:#000;color:#fff;font-family:system-ui}} video {{width:100%;max-height:90vh}}</style>
</head>
<body>
<h2>Playing: {}</h2>
<video id="player" controls autoplay></video>
<div id="playlist"></div>
<script>
const videos = [{}];
let current = 0;
const player = document.getElementById('player');
if (videos.length > 0) {{
    player.src = videos[0];
    player.onended = () => {{
        current++;
        if (current < videos.length) {{
            player.src = videos[current];
        }}
    }};
}}
</script>
</body>
</html>"#,
        folder_name, folder_name, js_array
    );

    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], html).into_response()
}

/// Stream MP4 file with range support - streams in 256MB chunks to handle any file size
async fn stream_handler(
    Path(encoded_path): Path<String>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Decode path
    let decoded = match decode_path(&encoded_path) {
        Some(d) => d,
        None => return (StatusCode::BAD_REQUEST, "Invalid path encoding").into_response(),
    };

    // Resolve full path
    let full_path = state.root_dir.join(decoded);
    if !full_path.exists() || !full_path.is_file() {
        return (StatusCode::NOT_FOUND, "File does not exist").into_response();
    }

    // Check if MP4
    if full_path.extension().is_none_or(|ext| ext != "mp4") {
        return (StatusCode::NOT_FOUND, "File is not an MP4").into_response();
    }

    // Get file size
    let metadata = match tokio::fs::metadata(&full_path).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read file metadata: {}", e),
            )
                .into_response();
        }
    };
    let file_size = metadata.len();

    // Fixed 2GB limit — now streams any size
    const CHUNK_SIZE: usize = 256 * 1024 * 1024; // 256MB chunks

    // Handle range
    let mut status = StatusCode::OK;
    let mut headers_resp = HeaderMap::new();
    headers_resp.insert(header::CONTENT_TYPE, "video/mp4".parse().unwrap());
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
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                "Requested range is invalid",
            )
                .into_response();
        }

        status = StatusCode::PARTIAL_CONTENT;
        headers_resp.insert(
            "Content-Range",
            format!("bytes {}-{}/{}", start, end, file_size)
                .parse()
                .unwrap(),
        );
        headers_resp.insert(
            header::CONTENT_LENGTH,
            (end - start + 1).to_string().parse().unwrap(),
        );
        (start, end)
    } else {
        headers_resp.insert(
            header::CONTENT_LENGTH,
            file_size.to_string().parse().unwrap(),
        );
        (0, file_size - 1)
    };

    // Create streaming body
    let stream = async_stream::stream! {
        let mut file = match tokio::fs::File::open(&full_path).await {
            Ok(f) => f,
            Err(e) => {
                yield Err(std::io::Error::other(format!("Failed to open file: {}", e)));
                return;
            }
        };

        if start_byte > 0
            && let Err(e) = file.seek(std::io::SeekFrom::Start(start_byte)).await {
                yield Err(std::io::Error::other(format!("Failed to seek file: {}", e)));
                return;
            }

        let mut remaining = (end_byte - start_byte + 1) as usize;
        while remaining > 0 {
            let chunk_size = std::cmp::min(CHUNK_SIZE, remaining);
            let mut buffer = vec![0; chunk_size];
            match file.read_exact(&mut buffer).await {
                Ok(_) => {
                    yield Ok(axum::body::Bytes::from(buffer));
                    remaining -= chunk_size;
                }
                Err(e) => {
                    yield Err(std::io::Error::other(format!("Failed to read file chunk: {}", e)));
                    return;
                }
            }
        }
    };

    let body = Body::from_stream(stream);
    (status, headers_resp, body).into_response()
}

#[derive(Parser)]
#[command(name = "theia-rust")]
#[command(about = "A simple MP4 file server")]
struct Args {
    /// Root directory to serve files from (default: ~/Theia_Home)
    #[arg(short, long)]
    root: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let root_dir = args.root.unwrap_or_else(|| {
        dirs::home_dir()
            .expect("Could not find home directory")
            .join("Theia_Home")
    });

    // Create directory if it doesn't exist
    if !root_dir.exists() {
        std::fs::create_dir_all(&root_dir).expect("Failed to create root directory");
    }

    println!("Serving from: {}", root_dir.display());

    let state = AppState { root_dir };

    let app = axum::Router::new()
        .route("/login", axum::routing::get(login_handler))
        .route("/", axum::routing::get(directory_handler))
        .route(
            "/playall/:encoded_path",
            axum::routing::get(playall_handler),
        )
        .route("/stream/:encoded_path", axum::routing::get(stream_handler))
        .layer(axum::middleware::from_fn(basic_auth_middleware))
        .with_state(state);

    let addr: std::net::SocketAddr = "0.0.0.0:32450".parse().unwrap();
    println!("Server running on http://localhost:32450");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
