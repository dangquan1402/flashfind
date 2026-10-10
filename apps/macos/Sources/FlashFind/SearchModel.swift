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
    var why: String = ""
}

/// `file.rs:12[:3]` at the end of a query → (12, 3).
func lineSuffix(_ q: String) -> (line: Int, col: Int?)? {
    guard let last = q.split(separator: " ").last,
          let m = String(last).wholeMatch(of: /([^:]*[^:0-9][^:]*):(\d+)(?::(\d+))?/),
          let line = Int(m.2) else { return nil }
    return (line, m.3.flatMap { Int($0) })
}

struct LaunchOptions {
    var roots: String?
    var cache: String?
    var diskIndex: String?
    var query: String?
    var snapshot: String?
    var windowIdFile: String?
    var quitAfter: Double?
    var cwd: String?

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
            case "--cwd": o.cwd = it.next()
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
    /// Hits marked with Tab (multi-select for copy / open).
    @Published var marked = Set<Int>()
    /// The list shows suggestions (nothing typed yet).
    @Published private(set) var suggesting = false
    @Published private(set) var status = "Loading projects…"
    @Published private(set) var hint: String?
    @Published private(set) var ready = false
    @Published private(set) var diskAvailable = false
    /// Current project (repo root) and how it was found.
    @Published private(set) var contextRoot: String?
    @Published private(set) var contextHow = ""
    private var pendingContext: (Int32, String, String?)?

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
                    if let cwd = o.cwd {
                        self.pendingContext = nil
                        self.setCwd(cwd)
                    } else if let p = self.pendingContext {
                        self.pendingContext = nil
                        self.setContext(pid: p.0, bundle: p.1, title: p.2)
                    }
                } else {
                    self.status = "Could not open projects: \(err ?? "unknown error")"
                }
                self.search()
            }
        }
    }

    /// The app that was frontmost when the launcher was summoned: its project
    /// becomes the ranking context (files there, uncommitted and recently
    /// committed ones first).
    func setContext(pid: Int32, bundle: String, title: String?) {
        guard let code else { pendingContext = (pid, bundle, title); return }
        queue.async {
            let d = code.detectContext(pid: pid, bundle: bundle, title: title)
            if let root = d.root { code.setCwd(root) }
            DispatchQueue.main.async {
                if let root = d.root {
                    self.contextRoot = root
                    self.contextHow = d.how
                }
                self.search()
            }
        }
    }

    /// Explicit context (e.g. --cwd on the command line).
    func setCwd(_ dir: String) {
        guard let code else { return }
        queue.async { code.setCwd(dir) }
        contextRoot = dir
        contextHow = "--cwd"
    }

    var contextName: String? { contextRoot.map { ($0 as NSString).lastPathComponent } }

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
        if q.isEmpty && mode == .disk {
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
            let r: SearchResult? = mode == .code ? code?.search(q, limit: q.isEmpty ? 30 : 80, cancel: tok) : disk?.search(q, limit: 80, cancel: tok)
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
        suggesting = mode == .code && query.trimmingCharacters(in: .whitespaces).isEmpty
        var rows: [Row] = []
        var lastGroup = -1
        for (i, h) in r.hits.enumerated() {
            if mode == .code && h.group != lastGroup {
                lastGroup = h.group
                rows.append(Row(id: -1 - h.group, header: true, title: h.repo, subtitle: "\(abbreviate(h.repoRoot)) · \(ago(h.repoActive))", hitIndex: nil, isDir: true))
            }
            rows.append(Row(id: i, header: false, title: h.rel, subtitle: "", hitIndex: i, isDir: h.isDir, why: h.why))
        }
        self.rows = rows
        selected = 0
        marked = []
        if suggesting {
            status = r.hits.isEmpty ? idle : "Suggested: recent, frequent and uncommitted files · type to search · " + brief
            onResults?()
            return
        }
        let engine = Double(r.engineMicros) / 1000
        let shown = r.total > UInt64(r.hits.count) ? "\(r.hits.count) of \(r.total.formatted())" : "\(r.hits.count)"
        if Date() > flashUntil {
            status = String(format: "%.2f ms engine · %.1f ms keystroke→results · ", engine, screenMs) + "\(shown) matches · " + (mode == .code ? brief : "whole disk")
        }
        onResults?()
    }

    var selectedHit: Hit? { hits.indices.contains(selected) ? hits[selected] : nil }

    /// Marked hits (Tab), else the selected one.
    var targets: [Hit] {
        let m = marked.sorted().compactMap { hits.indices.contains($0) ? hits[$0] : nil }
        return m.isEmpty ? (selectedHit.map { [$0] } ?? []) : m
    }

    func toggleMark() {
        guard selectedHit != nil else { return }
        if marked.contains(selected) { marked.remove(selected) } else { marked.insert(selected) }
        move(1)
    }

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
        for h in targets {
            learn(h, "open")
            NSWorkspace.shared.open(URL(fileURLWithPath: h.path))
        }
    }

    func reveal() {
        let t = targets
        guard !t.isEmpty else { return }
        for h in t { learn(h, "reveal") }
        NSWorkspace.shared.activateFileViewerSelecting(t.map { URL(fileURLWithPath: $0.path) })
    }

    static func cursorApp() -> URL? {
        if let u = NSWorkspace.shared.urlForApplication(withBundleIdentifier: "com.todesktop.230313mzl4w4u92") { return u }
        for p in ["/Applications/Cursor.app", home + "/Applications/Cursor.app"] where FileManager.default.fileExists(atPath: p) {
            return URL(fileURLWithPath: p)
        }
        return nil
    }

    static func vscodeApp() -> URL? {
        NSWorkspace.shared.urlForApplication(withBundleIdentifier: "com.microsoft.VSCode")
    }

    /// ⌥↩: open in Cursor (VS Code as a fallback), at `file:LINE[:COL]` when
    /// the query ends with one. The repo folder opens as the workspace.
    func openInCursor() {
        let at = lineSuffix(query)
        let cursor = SearchModel.cursorApp()
        let editor = cursor ?? SearchModel.vscodeApp()
        guard let editor else {
            flash("Cursor (or VS Code) is not installed: opening with the default app")
            open()
            return
        }
        let scheme = cursor != nil ? "cursor" : "vscode"
        for h in targets {
            learn(h, "cursor")
            var urls = [URL(fileURLWithPath: h.path)]
            if mode == .code && !h.repoRoot.isEmpty { urls.insert(URL(fileURLWithPath: h.repoRoot), at: 0) }
            NSWorkspace.shared.open(urls, withApplicationAt: editor, configuration: NSWorkspace.OpenConfiguration())
            if let at, !h.isDir {
                // cursor://file/<abs path>:<line>:<col> jumps to the line (VS Code URL handler).
                let enc = h.path.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? h.path
                if let u = URL(string: "\(scheme)://file\(enc):\(at.line):\(at.col ?? 1)") {
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { NSWorkspace.shared.open(u) }
                }
            }
        }
    }

    enum CopyKind { case absolute, relative, mention }

    /// ⌘C absolute paths · ⌘⇧C repo-relative · ⌥⌘C `@path` mentions for
    /// agent chats (Cursor / Claude Code), of the marked rows or the selected one.
    func copy(_ kind: CopyKind) {
        let t = targets
        guard !t.isEmpty else { return }
        let rel = { (h: Hit) in mode == .code && !h.rel.isEmpty ? h.rel : abbreviate(h.path) }
        let text: String
        switch kind {
        case .absolute: text = t.map(\.path).joined(separator: "\n")
        case .relative: text = t.map(rel).joined(separator: "\n")
        case .mention: text = t.map { "@" + rel($0) }.joined(separator: " ")
        }
        for h in t { learn(h, "copy") }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
        let what = t.count == 1 ? (kind == .absolute ? abbreviate(t[0].path) : text) : "\(t.count) paths"
        flash("Copied \(what)")
    }

    func copyPath() { copy(.absolute) }
}
