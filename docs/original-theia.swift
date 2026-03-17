//
//  TheiaApp.swift
//  Theia
//
//  Created by WEI on 7/15/25.
//

import SwiftUI
import AVFoundation
import Foundation
import Swifter
import AppKit

extension String {
    func htmlEscape() -> String {
        self.replacingOccurrences(of: "&", with: "&amp;")
            .replacingOccurrences(of: "<", with: "&lt;")
            .replacingOccurrences(of: ">", with: "&gt;")
            .replacingOccurrences(of: "\"", with: "&quot;")
            .replacingOccurrences(of: "'", with: "&#39;")
    }
}

func resolveURL(base: URL, relativePath: String) -> URL {
    relativePath.components(separatedBy: "/").reduce(base) { $0.appendingPathComponent($1) }
}

class SharedSettings: ObservableObject {
    @Published var serverStatus = "Starting server..."
    @Published var enableThumbnails = false
    @Published var memoryUsage: String = "Calculating..."
    var server: HttpServer? = nil
    var timer: Timer? = nil
    
    init() {
        timer = Timer.scheduledTimer(withTimeInterval: 5.0, repeats: true) { [weak self] _ in
            self?.memoryUsage = getMemoryUsage()
        }
        memoryUsage = getMemoryUsage()
    }
    
    deinit {
        timer?.invalidate()
    }
}

// Main App
@main
struct TheiaApp: App {
    @StateObject private var settings = SharedSettings()
    
    var body: some Scene {
        MenuBarExtra {
            VStack {
                Text(settings.serverStatus)
                Text("RAM: \(settings.memoryUsage)")
                Toggle("Enable Thumbnail Generation", isOn: $settings.enableThumbnails)
                Divider()
                Button("Restart Server") {
                    if let server = settings.server {
                        server.stop()
                        settings.serverStatus = "Restarting server..."
                        settings.server = nil
                    }
                    startServer(settings: settings)
                }
                Button("Quit") {
                    NSApplication.shared.terminate(nil)
                }
            }
            .padding()
            .onAppear {
                startServer(settings: settings)
            }
        } label: {
            Image("TheiaIcon")
                .resizable()
                .scaledToFit()
                .frame(width: 20, height: 20)
        }
    }
    
    func startServer(settings: SharedSettings) {
        DispatchQueue.global().async {
            print("Debug: Entering global async block")
            do {
                print("Debug: Creating HttpServer")
                let newServer = HttpServer()
                
                // Create Theia_Home if not exists
                let fm = FileManager.default
                let root = fm.homeDirectoryForCurrentUser.appendingPathComponent("Theia_Home")
                if !fm.fileExists(atPath: root.path) {
                    try fm.createDirectory(at: root, withIntermediateDirectories: true)
                }
                
                print("Debug: Adding middleware")
                // Basic auth middleware, exempt /login
                newServer.middleware.append { request in
                    if request.path == "/login" {
                        return nil
                    }
                    guard let auth = request.headers["authorization"],
                          let data = Data(base64Encoded: auth.replacingOccurrences(of: "Basic ", with: "")),
                          let credentials = String(data: data, encoding: .utf8),
                          credentials == "theia:theia" else {
                        let headers = ["WWW-Authenticate": "Basic realm=\"Theia\""]
                        return HttpResponse.raw(401, "Unauthorized", headers) { writer in
                            try? writer.write(Data("Authentication required.".utf8))
                        }
                    }
                    return nil
                }
                
                print("Debug: Setting /login route")
                // Login page with instructions
                newServer["/login"] = { request in
                    return HttpResponse.ok(.html("""
                    <!DOCTYPE html>
                    <html>
                    <head>
                    <meta charset="UTF-8">
                    <title>Theia</title>
                    </head>
                    <body>
                    <h1>Theia</h1>
                    <p>To access the library, visit <a href="/">/</a>. When prompted, use username: theia, password: theia.</p>
                    </body>
                    </html>
                    """))
                }
                
                print("Debug: Setting / route")
                // Root: media list
                newServer["/"] = { request in
                    let mediaTree = scanMedia(at: root, virtualPrefix: "")
                    let css = """
                    body { font-family: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif, "Apple Color Emoji", "Segoe UI Emoji", "Segoe UI Symbol"; }
                    ul { list-style: none; padding-left: 0; }
                    li { margin-bottom: 10px; }
                    .item { display: flex; align-items: center; }
                    .thumb { width: 110px; flex-shrink: 0; text-align: center; }
                    .thumb img { width: 100px; height: auto; border-radius: 22%; }
                    .name { flex: 1; word-break: break-word; }
                    """
                    var listHtml = ""
                    var isEmpty = true
                    if case .folder(_, let children) = mediaTree {
                        listHtml += "<ul>"
                        for child in children {
                            listHtml += buildHTML(for: child)
                        }
                        listHtml += "</ul>"
                        isEmpty = children.isEmpty
                    }
                    var html = """
                    <!DOCTYPE html>
                    <html>
                    <head>
                    <meta charset="UTF-8">
                    <title>Theia Media Library</title>
                    <style>\(css)</style>
                    </head>
                    <body>
                    \(listHtml)
                    """
                    if isEmpty {
                        html += "<p>No media found. Add files to ~/Theia_Home.</p>"
                    }
                    html += "</body></html>"
                    return HttpResponse.ok(.html(html))
                }
                
                print("Debug: Setting /stream route")
                // Stream file with range support
                newServer["/stream/*"] = { request in
                    let encodedPath = String(request.path.dropFirst("/stream/".count))
                    guard let path = encodedPath.removingPercentEncoding else { return .badRequest(nil) }
                    let fullURL = resolveURL(base: root, relativePath: path)
                    let fullPath = fullURL.path
                    guard FileManager.default.fileExists(atPath: fullPath) else { return .notFound }
                    
                    var headers: [String: String] = ["Content-Type": mimeType(for: fullPath)]
                    let fileSize = (try? FileManager.default.attributesOfItem(atPath: fullPath)[.size] as? Int64) ?? 0
                    
                    if let rangeHeader = request.headers["range"], rangeHeader.hasPrefix("bytes=") {
                        let ranges = rangeHeader.replacingOccurrences(of: "bytes=", with: "").split(separator: "-")
                        let start = Int64(ranges[0]) ?? 0
                        let end = ranges.count > 1 ? (Int64(ranges[1]) ?? fileSize - 1) : fileSize - 1
                        let length = end - start + 1
                        
                        headers["Content-Range"] = "bytes \(start)-\(end)/\(fileSize)"
                        headers["Content-Length"] = "\(length)"
                        headers["Accept-Ranges"] = "bytes"
                        
                        return HttpResponse.raw(206, "Partial Content", headers) { writer in
                            guard let fileHandle = try? FileHandle(forReadingFrom: fullURL) else { return }
                            defer { try? fileHandle.close() }
                            try? fileHandle.seek(toOffset: UInt64(start))
                            var remaining = length
                            while remaining > 0 {
                                let chunkSize = min(256 * 1024 * 1024, Int(remaining))
                                if let chunk = try? fileHandle.read(upToCount: chunkSize), !chunk.isEmpty {
                                    try? writer.write(chunk)
                                    remaining -= Int64(chunk.count)
                                } else {
                                    break
                                }
                            }
                        }
                    } else {
                        headers["Content-Length"] = "\(fileSize)"
                        headers["Accept-Ranges"] = "bytes"
                        
                        return HttpResponse.raw(200, "OK", headers) { writer in
                            guard let fileHandle = try? FileHandle(forReadingFrom: fullURL) else { return }
                            defer { try? fileHandle.close() }
                            while let chunk = try? fileHandle.read(upToCount: 256 * 1024 * 1024), !chunk.isEmpty {
                                try? writer.write(chunk)
                            }
                        }
                    }
                }
                
                print("Debug: Setting /thumb route")
                // Thumb
                newServer["/thumb/*"] = { request in
                    let encodedPath = String(request.path.dropFirst("/thumb/".count))
                    guard let path = encodedPath.removingPercentEncoding else { return .badRequest(nil) }
                    let thumbPath = generateThumbnail(settings: settings, for: path)
                    if thumbPath.isEmpty {
                        return .notFound
                    }
                    if let data = try? Data(contentsOf: URL(fileURLWithPath: thumbPath)) {
                        return .ok(.data(data, contentType: "image/png"))
                    }
                    return .internalServerError
                }
                
                print("Debug: About to start server")
                try newServer.start(32450, forceIPv4: true)
                print("Debug: Server started successfully")
                
                DispatchQueue.main.async {
                    settings.server = newServer
                    settings.serverStatus = "Server running on http://localhost:32450"
                }
            } catch {
                print("Debug: Error caught: \(error)")
                DispatchQueue.main.async {
                    settings.serverStatus = "Error: \(error)"
                }
            }
        }
    }
}

// MIME type function
func mimeType(for path: String) -> String {
    let ext = (path as NSString).pathExtension.lowercased()
    switch ext {
    case "mp4": return "video/mp4"
    case "mkv": return "video/x-matroska"
    case "avi": return "video/x-msvideo"
    case "mov": return "video/quicktime"
    case "mp3": return "audio/mpeg"
    default: return "application/octet-stream"
    }
}

// Media Node
enum MediaNode {
    case file(name: String, path: String, thumbnail: String)
    case folder(name: String, children: [MediaNode])
}

// Scan recursively
func scanMedia(at url: URL, virtualPrefix: String = "", displayName: String? = nil) -> MediaNode {
    print("Debug: Entering scanMedia for URL: \(url.path), virtualPrefix: \(virtualPrefix), displayName: \(displayName ?? "nil")")
    let fm = FileManager.default
    let name = displayName ?? url.lastPathComponent
    var children: [MediaNode] = []
    
    do {
        let contents = try fm.contentsOfDirectory(at: url, includingPropertiesForKeys: nil)
        print("Debug: Found \(contents.count) items in \(url.path)")
        for item in contents.sorted(by: { $0.lastPathComponent < $1.lastPathComponent }) {
            print("Debug: Processing item: \(item.lastPathComponent)")
            if item.lastPathComponent.hasPrefix(".") {
                print("Debug: Skipping hidden item: \(item.lastPathComponent)")
                continue
            }
            let resolvedItem = item.resolvingSymlinksInPath()
            print("Debug: Resolved item to: \(resolvedItem.path)")
            var isDir: ObjCBool = false
            if fm.fileExists(atPath: resolvedItem.path, isDirectory: &isDir) {
                print("Debug: Item exists, isDirectory: \(isDir.boolValue)")
                if isDir.boolValue {
                    let childPrefix = virtualPrefix + item.lastPathComponent + "/"
                    print("Debug: Recursing into directory with prefix: \(childPrefix)")
                    children.append(scanMedia(at: resolvedItem, virtualPrefix: childPrefix, displayName: item.lastPathComponent))
                } else {
                    let rawRelPath = virtualPrefix + item.lastPathComponent
                    print("Debug: Adding file with rawRelPath: \(rawRelPath)")
                    let pathComponents = rawRelPath.components(separatedBy: "/").filter { !$0.isEmpty }
                    let encodedComponents = pathComponents.map { $0.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? $0 }
                    let encodedRelPath = encodedComponents.joined(separator: "%2F")
                    let streamPath = "/stream/" + encodedRelPath
                    let thumbnail = "/thumb/" + encodedRelPath
                    children.append(.file(name: item.lastPathComponent, path: streamPath, thumbnail: thumbnail))
                }
            } else {
                print("Debug: Item does not exist: \(resolvedItem.path)")
            }
        }
    } catch {
        print("Debug: Error reading contents of \(url.path): \(error.localizedDescription)")
        print("Debug: Failed to read contents of \(url.path)")
    }
    print("Debug: Exiting scanMedia for \(url.path) with \(children.count) children")
    return .folder(name: name, children: children)
}

// Build HTML
func buildHTML(for node: MediaNode) -> String {
    switch node {
    case .file(let name, let path, let thumbnail):
        return "<li class=\"item\"><div class=\"thumb\"><img src=\"\(thumbnail.htmlEscape())\"></div><div class=\"name\"><a href=\"\(path.htmlEscape())\">\(name.htmlEscape())</a></div></li>"
    case .folder(let name, let children):
        var html = "<li class=\"item\"><div class=\"thumb\"></div><div class=\"name\">\(name.htmlEscape())</div></li>"
        if !children.isEmpty {
            html += "<li><ul>"
            for child in children {
                html += buildHTML(for: child)
            }
            html += "</ul></li>"
        }
        return html
    }
}

// Generate thumbnail
func generateThumbnail(settings: SharedSettings, for filePath: String) -> String {
    let fm = FileManager.default
    let root = fm.homeDirectoryForCurrentUser.appendingPathComponent("Theia_Home")
    let url = resolveURL(base: root, relativePath: filePath)
    let thumbsBase = root.appendingPathComponent(".thumbs")
    let thumbDir = resolveURL(base: thumbsBase, relativePath: filePath).deletingLastPathComponent()
    try? fm.createDirectory(at: thumbDir, withIntermediateDirectories: true)
    let thumbURL = thumbDir.appendingPathComponent(url.lastPathComponent + ".png")
    let thumbPath = thumbURL.path
    
    if fm.fileExists(atPath: thumbPath) { return thumbPath }
    
    if !settings.enableThumbnails { return "" }
    
    let asset = AVURLAsset(url: url)
    let generator = AVAssetImageGenerator(asset: asset)
    generator.appliesPreferredTrackTransform = true
    if let cgImage = try? generator.copyCGImage(at: .zero, actualTime: nil) {
        let image = NSImage(cgImage: cgImage, size: NSSize(width: 100, height: 100))
        if let data = image.tiffRepresentation, let bitmap = NSBitmapImageRep(data: data), let png = bitmap.representation(using: .png, properties: [:]) {
            try? png.write(to: thumbURL)
        }
    }
    return thumbPath
}

func getMemoryUsage() -> String {
    var taskInfo = task_vm_info_data_t()
    var count = mach_msg_type_number_t(MemoryLayout<task_vm_info_data_t>.size) / 4
    let result: kern_return_t = withUnsafeMutablePointer(to: &taskInfo) {
        $0.withMemoryRebound(to: integer_t.self, capacity: 1) {
            task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count)
        }
    }
    let used: UInt64 = result == KERN_SUCCESS ? UInt64(taskInfo.phys_footprint) : 0
    let usedMB = Double(used) / 1024 / 1024
    return String(format: "%.1f MB", usedMB)
}
