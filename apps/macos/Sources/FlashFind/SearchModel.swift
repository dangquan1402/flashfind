import AppKit
import CFlashFind
import Foundation
import SwiftUI

enum Mode: String { case code, disk }

struct Row: Identifiable {
    let id: Int
    let header: Bool
    let title: String
    let subtitle: String
    let hitIndex: Int?
    let isDir: Bool
}

struct LaunchOptions {
    var roots: String?
    var cache: String?
    var diskIndex: String?
    var query: String?
    var snapshot: String?
    var windowIdFile: String?
    var quitAfter: Double?

    static func parse(_ args: [String]) -> LaunchOptions {
        var o = LaunchOptions()
        var it = args.dropFirst().makeIterator()
        while let a = it.next() {
            switch a {
            case "--roots": o.roots = it.next()
            case "--cache": o.cache = it.next()
            case "--disk-index": o.diskIndex = it.next()
            case "--query": o.query = it.next()
            case "--snapshot": o.snapshot = it.next()
            case "--window-id-file": o.windowIdFile = it.next()
            case "--quit-after": o.quitAfter = it.next().flatMap(Double.init)
            default: break
            }
        }
        return o
    }
}

func ago(_ t: Int64) -> String {
    if t <= 0 { return "no commits" }
    let d = max(0, Int64(Date().timeIntervalSince1970) - t)
    switch d {
    case ..<120: return "just now"
    case ..<7200: return "\(d / 60) min ago"
    case ..<172_800: return "\(d / 3600) h ago"
    default: return "\(d / 86400) d ago"
    }
}

final class SearchModel: ObservableObject {
    @Published var query = ""
    @Published var mode: Mode = .code
    @Published private(set) var hits: [Hit] = []
    @Published private(set) var rows: [Row] = []
    @Published var selected = 0
    @Published private(set) var status = "Loading projects…"
    @Published private(set) var hint: String?
    @Published private(set) var ready = false
    @Published private(set) var diskAvailable = false

    let opts: LaunchOptions
    var onResults: (() -> Void)?
    private var code: CodeEngine?
    private var disk: DiskEngine?
    private let queue = DispatchQueue(label: "dev.flashfind.search", qos: .userInteractive)
    /// Token of the newest search; cancelled when the next keystroke arrives.
    private var current: OpaquePointer?
    private var seq = 0
    private var idle = ""
    private var brief = ""
    private var flashUntil = Date.distantPast

    init(opts: LaunchOptions) {
        self.opts = opts
        if let q = opts.query { query = q }
    }

    var diskIndexPath: String { opts.diskIndex ?? home + "/.flashfind/index.ffx" }

    func load() {
        let o = opts
        let diskPath = diskIndexPath
        DispatchQueue.global(qos: .userInitiated).async {
            let t0 = Date()
            var err: String?
            let c = CodeEngine(roots: o.roots, cache: o.cache, error: &err)
            let watchErr = c?.watch()
            let ms = Date().timeIntervalSince(t0) * 1000
            let d = FileManager.default.fileExists(atPath: diskPath) ? DiskEngine(path: diskPath) : nil
            DispatchQueue.main.async {
                self.code = c
                self.disk = d
                self.diskAvailable = d != nil
                self.ready = c != nil
                if let c {
                    let s = c.stats()
                    let live = watchErr == nil ? "live" : "not watching"
                    self.idle = "\(s.repos) projects · \(s.entries.formatted()) files · opened in \(Int(ms)) ms · \(live)"
                    self.brief = "\(s.repos) projects · \(live)"
                    self.status = self.idle
                } else {
                    self.status = "Could not open projects: \(err ?? "unknown error")"
                }
                self.search()
            }
        }
    }

    func modeChanged() {
        hits = []
        rows = []
        search()
    }

    /// Called on every keystroke: cancel the previous search, start a new one.
    func search() {
        seq += 1
        let my = seq
        if let c = current { ff_cancel_cancel(c) }
        guard ready else { return }
        let q = query.trimmingCharacters(in: .whitespaces)
        hint = nil
        if mode == .disk && disk == nil {
            hits = []
            rows = []
            hint = "Whole-disk mode needs an index at \(abbreviate(diskIndexPath)).\nBuild one with:  flashfind index / -o ~/.flashfind/index.ffx\n(the terminal needs Full Disk Access for a complete crawl)."
            return
        }
        if q.isEmpty {
            hits = []
            rows = []
            status = idle
            onResults?()
            return
        }
        guard let tok = ff_cancel_new() else { return }
        current = tok
        let mode = self.mode
        let code = self.code
        let disk = self.disk
        let t0 = DispatchTime.now().uptimeNanoseconds
        queue.async {
            let r: SearchResult? = mode == .code ? code?.search(q, limit: 80, cancel: tok) : disk?.search(q, limit: 80, cancel: tok)
            DispatchQueue.main.async {
                // Each token is freed exactly once, after its own search returned.
                if self.current == tok { self.current = nil }
                ff_cancel_free(tok)
                guard let r, my == self.seq else { return }
                let screenMs = Double(DispatchTime.now().uptimeNanoseconds - t0) / 1e6
                self.apply(r, screenMs: screenMs)
            }
        }
    }

    private func apply(_ r: SearchResult, screenMs: Double) {
        if let e = r.error {
            hits = []
            rows = []
            hint = e
            status = idle
            onResults?()
            return
        }
        hits = r.hits
        var rows: [Row] = []
        var lastGroup = -1
        for (i, h) in r.hits.enumerated() {
            if mode == .code && h.group != lastGroup {
                lastGroup = h.group
                rows.append(Row(id: -1 - h.group, header: true, title: h.repo, subtitle: "\(abbreviate(h.repoRoot)) · \(ago(h.repoActive))", hitIndex: nil, isDir: true))
            }
            rows.append(Row(id: i, header: false, title: h.rel, subtitle: "", hitIndex: i, isDir: h.isDir))
        }
        self.rows = rows
        selected = 0
        let engine = Double(r.engineMicros) / 1000
        let shown = r.total > UInt64(r.hits.count) ? "\(r.hits.count) of \(r.total.formatted())" : "\(r.hits.count)"
        if Date() > flashUntil {
            status = String(format: "%.2f ms engine · %.1f ms keystroke→results · ", engine, screenMs) + "\(shown) matches · " + (mode == .code ? brief : "whole disk")
        }
        onResults?()
    }

    var selectedHit: Hit? { hits.indices.contains(selected) ? hits[selected] : nil }

    func move(_ d: Int) {
        guard !hits.isEmpty else { return }
        selected = max(0, min(hits.count - 1, selected + d))
    }

    func flash(_ s: String) {
        status = s
        flashUntil = Date().addingTimeInterval(1.5)
    }

    /// Every action on a result teaches the ranking which file the query meant.
    private func learn(_ h: Hit, _ action: String) {
        guard mode == .code, let code else { return }
        let q = query
        queue.async { code.recordPick(query: q, path: h.path, action: action) }
    }

    func open() {
        guard let h = selectedHit else { return }
        learn(h, "open")
        NSWorkspace.shared.open(URL(fileURLWithPath: h.path))
    }

    func reveal() {
        guard let h = selectedHit else { return }
        learn(h, "reveal")
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: h.path)])
    }

    static func cursorApp() -> URL? {
        if let u = NSWorkspace.shared.urlForApplication(withBundleIdentifier: "com.todesktop.230313mzl4w4u92") { return u }
        for p in ["/Applications/Cursor.app", home + "/Applications/Cursor.app"] where FileManager.default.fileExists(atPath: p) {
            return URL(fileURLWithPath: p)
        }
        return nil
    }

    func openInCursor() {
        guard let h = selectedHit else { return }
        guard let app = SearchModel.cursorApp() else {
            flash("Cursor is not installed")
            return
        }
        learn(h, "cursor")
        var urls = [URL(fileURLWithPath: h.path)]
        if mode == .code && !h.repoRoot.isEmpty { urls.insert(URL(fileURLWithPath: h.repoRoot), at: 0) }
        NSWorkspace.shared.open(urls, withApplicationAt: app, configuration: NSWorkspace.OpenConfiguration())
    }

    func copyPath() {
        guard let h = selectedHit else { return }
        learn(h, "copy")
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(h.path, forType: .string)
        flash("Copied \(abbreviate(h.path))")
    }
}
