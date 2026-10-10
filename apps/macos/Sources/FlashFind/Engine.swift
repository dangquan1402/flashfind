// Thin Swift wrappers over the flashfind C ABI. Everything runs in-process:
// no helper binary, no sockets.
import CFlashFind
import Foundation

struct Hit: Hashable {
    let path: String
    /// Path shown in the list: relative to the repo (code) or ~-abbreviated (disk).
    let rel: String
    let repo: String
    let repoRoot: String
    let group: Int
    let isDir: Bool
    let repoActive: Int64
    /// Why personal ranking moved it ("picked before", "dirty", …).
    var why: String = ""
}

struct SearchResult {
    var hits: [Hit] = []
    var total: UInt64 = 0
    var engineMicros: UInt64 = 0
    var error: String?
}

private final class Box { var r = SearchResult() }

let home = NSHomeDirectory()

func abbreviate(_ p: String) -> String {
    p.hasPrefix(home) ? "~" + p.dropFirst(home.count) : p
}

private func takeError(_ e: UnsafeMutablePointer<CChar>?) -> String? {
    guard let e else { return nil }
    let s = String(cString: e)
    ff_string_free(e)
    return s
}

extension Optional where Wrapped == String {
    func withOptionalCString<R>(_ body: (UnsafePointer<CChar>?) -> R) -> R {
        switch self {
        case .some(let s): return s.withCString { body($0) }
        case .none: return body(nil)
        }
    }
}

private let codePageCallback: ff_code_page_cb = { ctx, page in
    guard let ctx, let page else { return }
    let box = Unmanaged<Box>.fromOpaque(ctx).takeUnretainedValue()
    let p = page.pointee
    box.r.total = p.total
    box.r.engineMicros = p.elapsed_us
    var hits: [Hit] = []
    hits.reserveCapacity(p.count)
    for i in 0..<p.count {
        let h = p.hits[i]
        hits.append(Hit(path: String(cString: h.path), rel: String(cString: h.rel), repo: String(cString: h.repo),
                        repoRoot: String(cString: h.repo_root), group: Int(h.group), isDir: h.kind == 1,
                        repoActive: h.repo_active, why: h.why.map { String(cString: $0) } ?? ""))
    }
    box.r.hits = hits
}

private let diskPageCallback: ff_page_cb = { ctx, page in
    guard let ctx, let page else { return }
    let box = Unmanaged<Box>.fromOpaque(ctx).takeUnretainedValue()
    let p = page.pointee
    box.r.total = p.total
    box.r.engineMicros = p.elapsed_us
    var hits: [Hit] = []
    hits.reserveCapacity(p.count)
    for i in 0..<p.count {
        let h = p.hits[i]
        let path = String(cString: h.path)
        hits.append(Hit(path: path, rel: abbreviate(path), repo: "", repoRoot: "", group: 0, isDir: h.kind == 1, repoActive: 0))
    }
    box.r.hits = hits
}

/// Code projects: git repos under the roots, gitignore-aware, live-updated.
final class CodeEngine: @unchecked Sendable {
    private let code: OpaquePointer
    private let session: OpaquePointer

    init?(roots: String?, cache: String?, error: inout String?) {
        var err: UnsafeMutablePointer<CChar>?
        let c: OpaquePointer? = roots.withOptionalCString { r in cache.withOptionalCString { cc in ff_code_open(r, cc, &err) } }
        guard let c else {
            error = takeError(err) ?? "ff_code_open failed"
            return nil
        }
        code = c
        session = ff_code_session_new(c)
    }

    deinit {
        ff_code_session_free(session)
        ff_code_free(code)
    }

    func watch() -> String? {
        var err: UnsafeMutablePointer<CChar>?
        return ff_code_watch(code, &err) == FF_OK ? nil : takeError(err)
    }

    func stats() -> ff_code_stats {
        var s = ff_code_stats()
        ff_code_get_stats(code, &s)
        return s
    }

    /// Current project for ranking (nil clears).
    func setCwd(_ path: String?) {
        path.withOptionalCString { ff_code_session_set_cwd(session, $0) }
    }

    /// Guess the project of the app that was frontmost (no extra permissions).
    func detectContext(pid: Int32, bundle: String, title: String?) -> (root: String?, how: String) {
        let raw = bundle.withCString { b in title.withOptionalCString { t in ff_code_detect_context(code, pid, b, t) } }
        guard let raw else { return (nil, "") }
        defer { ff_string_free(raw) }
        let data = Data(String(cString: raw).utf8)
        let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        return (obj?["root"] as? String, obj?["how"] as? String ?? "")
    }

    /// Learned ranking: remember the file meant for `query`.
    func recordPick(query: String, path: String, action: String) {
        _ = query.withCString { q in path.withCString { p in action.withCString { a in ff_code_record_pick(code, q, p, a) } } }
    }

    /// nil when cancelled. Call from one worker queue (the session is serial).
    func search(_ q: String, limit: UInt32, cancel: OpaquePointer) -> SearchResult? {
        let box = Box()
        var err: UnsafeMutablePointer<CChar>?
        let ctx = Unmanaged.passUnretained(box).toOpaque()
        let rc = q.withCString { ff_code_search(session, $0, limit, cancel, codePageCallback, ctx, &err) }
        if rc == FF_CANCELLED { return nil }
        if rc != FF_OK { box.r.error = takeError(err) ?? "search failed" }
        return box.r
    }
}

/// Whole disk: a saved flashfind index (built by `flashfind index /`).
final class DiskEngine: @unchecked Sendable {
    private let index: OpaquePointer
    private let session: OpaquePointer
    let entries: UInt64

    init?(path: String) {
        var err: UnsafeMutablePointer<CChar>?
        guard let i = path.withCString({ ff_index_open($0, &err) }) else {
            _ = takeError(err)
            return nil
        }
        index = i
        session = ff_session_new(i)
        entries = ff_index_entries(i)
    }

    deinit {
        ff_session_free(session)
        ff_index_free(index)
    }

    func search(_ q: String, limit: UInt32, cancel: OpaquePointer) -> SearchResult? {
        let box = Box()
        var err: UnsafeMutablePointer<CChar>?
        let ctx = Unmanaged.passUnretained(box).toOpaque()
        let rc = q.withCString { ff_search(session, $0, limit, cancel, diskPageCallback, ctx, &err) }
        if rc == FF_CANCELLED { return nil }
        if rc != FF_OK { box.r.error = takeError(err) ?? "search failed" }
        return box.r
    }
}
