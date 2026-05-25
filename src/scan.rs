//! Filesystem scanning, path encoding/decoding, and path-containment safety.
use crate::config::Config;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Tree node representing media files and folders.
#[derive(Debug)]
pub enum MediaNode {
    File {
        name: String,
        /// URL-safe encoded relative path (used for /stream, /play, /meta).
        enc: String,
        size: u64,
    },
    Folder {
        name: String,
        /// URL-safe encoded relative path (empty for the root folder).
        enc: String,
        children: Vec<MediaNode>,
    },
}

/// Encode path components with %2F separators for URL-safe nested paths.
pub fn encode_path(path: &str) -> String {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|comp| urlencoding::encode(comp))
        .collect::<Vec<_>>()
        .join("%2F")
}

/// Decode a %2F-separated path back to a normal relative filesystem path.
pub fn decode_path(encoded: &str) -> Option<String> {
    urlencoding::decode(encoded).ok().map(|s| s.to_string())
}

/// Resolve `decoded` against `root` and confirm the result stays inside `root`.
///
/// Returns the canonicalized absolute path only when it exists and is contained
/// within the (canonicalized) root directory. This blocks `../` traversal and
/// absolute-path injection. Returns `None` otherwise.
pub fn resolve_within(root: &Path, decoded: &str) -> Option<PathBuf> {
    let candidate = root.join(decoded);
    let canon = std::fs::canonicalize(&candidate).ok()?;
    let canon_root = std::fs::canonicalize(root).ok()?;
    if canon.starts_with(&canon_root) {
        Some(canon)
    } else {
        None
    }
}

/// Recursively scan the media directory, building a tree of media files and folders.
pub fn scan_media(root: &Path, prefix: &str, cfg: &Config) -> MediaNode {
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
        let rel_path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };

        if entry.path().is_dir() {
            let child = scan_media(entry.path(), &rel_path, cfg);
            // Skip folders that contain no media at any depth.
            if has_media(&child) {
                children.push(child);
            }
        } else if cfg.is_media(entry.path()) {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            children.push(MediaNode::File {
                name,
                enc: encode_path(&rel_path),
                size,
            });
        }
    }

    children.sort_by_key(|node| match node {
        MediaNode::File { name, .. } => name.to_lowercase(),
        MediaNode::Folder { name, .. } => name.to_lowercase(),
    });

    let name = root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    MediaNode::Folder {
        name,
        enc: encode_path(prefix),
        children,
    }
}

/// True if the node is (or contains, at any depth) at least one media file.
fn has_media(node: &MediaNode) -> bool {
    match node {
        MediaNode::File { .. } => true,
        MediaNode::Folder { children, .. } => children.iter().any(has_media),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_round_trips_nested_paths() {
        let enc = encode_path("Movies/My Film.webm");
        assert_eq!(enc, "Movies%2FMy%20Film.webm");
        // Axum decodes %2F before our handler sees it, so decode_path receives slashes.
        assert_eq!(decode_path("Movies/My Film.webm").unwrap(), "Movies/My Film.webm");
    }

    #[test]
    fn resolve_within_blocks_traversal() {
        let tmp = std::env::temp_dir().join("theia_scan_test");
        let _ = std::fs::create_dir_all(&tmp);
        let file = tmp.join("ok.mp4");
        std::fs::write(&file, b"x").unwrap();

        assert!(resolve_within(&tmp, "ok.mp4").is_some());
        assert!(resolve_within(&tmp, "../../etc/passwd").is_none());
        assert!(resolve_within(&tmp, "/etc/passwd").is_none());
        assert!(resolve_within(&tmp, "does-not-exist.mp4").is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
