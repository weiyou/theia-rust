**Cline Instruction**: Before ANY /act or /deep-planning, always read and strictly follow @docs/implementation_plan.md. This is the living architecture document. Never deviate without updating it first.

# Implementation Plan - Current State & Future Features

[Overview]
Successfully rebuilt the Theia Swift app as a portable Rust CLI that serves MP4 files from a specified directory with directory listing, video streaming, and playlist capabilities. **FIXED 2GB LIMIT** - now streams any size video using 256MB chunked streaming instead of loading entire files into memory.

The implementation creates a single binary HTTP server using Axum framework that serves only MP4 files from ~/Theia_Home or a custom root directory specified via --root flag. It includes basic authentication for all routes except /login with proper WWW-Authenticate header for browser popup, a /login page with authentication instructions, an HTML directory tree view at root path without thumbnails, byte-range supported streaming for MP4 files at /stream/{encoded-path} with per-segment %2F encoding for nested paths, and a playlist feature at /playall/{encoded-folder-path} that auto-plays all MP4s in a folder sequentially. The server runs on port 32450 by default and prints the running message. All code is contained in single src/main.rs file. Dependencies include axum, tokio, walkdir, clap, dirs, base64, urlencoding, and async-stream for cross-platform compatibility, proper encoding, and unlimited-size streaming.

[Types]
Current data structures implemented in single main.rs file.

- `MediaNode` enum: Variants `File { name: String, path: String }` and `Folder { name: String, path: String, children: Vec<MediaNode> }` for representing the file tree structure with encoded paths for links.
- `AppState` struct: Contains `root_dir: PathBuf` for shared server state.

[Files]
Current file structure with all code in single main.rs.

- `theia-rust/Cargo.toml`: Dependencies axum = "0.7", tokio = { version = "1", features = ["full"] }, walkdir = "2", clap = { version = "4", features = ["derive"] }, dirs = "5", base64 = "0.21", urlencoding = "2".
- `theia-rust/src/main.rs`: Complete server implementation including CLI parsing, auth middleware, directory scanning, HTML generation, streaming with byte-range support, and playlist functionality.

[Functions]
Current implementation functions in main.rs.

- `main()`: Parse CLI args, set up app state, configure routes with auth middleware, start server.
- `basic_auth_middleware()`: Tower middleware checking basic auth headers, exempting /login, returning 401 with WWW-Authenticate.
- `login_handler()`: Returns HTML page with authentication instructions.
- `directory_handler()`: Generates HTML directory tree from root, filtering MP4 files, with "▶ Play All" links for folders.
- `stream_handler()`: Serves MP4 files with full byte-range support (206 responses).
- `playall_handler()`: Scans folder for MP4 files, returns HTML page with JS auto-playing sequential videos.
- `scan_media()`: Recursively scans directory tree, building MediaNode structure with encoded paths.
- `build_html()`: Generates HTML for MediaNode tree with proper links.
- `generate_html_listing()`: Creates full HTML page for directory listing.
- `encode_path()`: URL-encodes paths with %2F separators for nested paths.
- `decode_path()`: URL-decodes paths from URLs to filesystem paths.

[Classes]
No class-based structures as Rust uses structs and enums.

[Dependencies]
Current dependencies in Cargo.toml.

- axum = "0.7" for HTTP server framework.
- tokio = { version = "1", features = ["full"] } for async runtime.
- walkdir = "2" for directory traversal.
- clap = { version = "4", features = ["derive"] } for CLI parsing.
- dirs = "5" for cross-platform home directory.
- base64 = "0.21" for auth decoding.
- urlencoding = "2" for path encoding/decoding.

[Testing]
No tests implemented yet - server functionality verified manually.

[Implementation Order]
Completed implementation sequence.

1. ✅ Update Cargo.toml with dependencies.
2. ✅ Implement CLI argument parsing in main.rs.
3. ✅ Create path encoding/decoding functions.
4. ✅ Implement basic auth middleware with WWW-Authenticate.
5. ✅ Create directory listing handler with HTML tree.
6. ✅ Add streaming handler with byte-range support.
7. ✅ Add playlist feature with /playall/ route.
8. ✅ Integrate all in single main.rs and test.

## Next Possible Features (Priority Order)

1. **File Upload Support**: Add POST /upload route to upload MP4 files to folders, with auth and size limits.
2. **File Deletion**: Add DELETE /stream/{encoded-path} route to remove files, with confirmation.
3. **Search Functionality**: Add /search?q=query route returning filtered directory listing.
4. **File Renaming**: Add PUT /stream/{encoded-path} with new name in body.
5. **Folder Creation**: Add POST /create-folder with folder name.
6. **File Move/Copy**: Add routes to move files between folders.
7. **Metadata Display**: Show file size, duration, resolution in directory listing.
8. **Sorting Options**: Add query params for sorting by name, date, size.
9. **Pagination**: For large directories, add page navigation.
10. **HTTPS Support**: Add TLS certificate options for secure serving.
11. **Rate Limiting**: Prevent abuse with request rate limits.
12. **Logging**: Add request logging to stdout or file.
13. **Configuration File**: Support config file for auth, port, root dir.
14. **WebSocket Status**: Real-time connection status updates.
15. **Bulk Operations**: Select multiple files for batch delete/move.
