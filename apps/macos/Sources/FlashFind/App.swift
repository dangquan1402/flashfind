// FlashFind: a launcher-style window over the in-process flashfind engine.
// Menu-bar item + ⌃⌥Space global hotkey (Carbon RegisterEventHotKey: no
// Accessibility permission needed). No Dock icon (LSUIElement).
import AppKit
import Carbon.HIToolbox
import Quartz
import SwiftUI

final class QuickLookSource: NSObject, QLPreviewPanelDataSource, QLPreviewPanelDelegate {
    var url: URL?

    func numberOfPreviewItems(in panel: QLPreviewPanel!) -> Int { url == nil ? 0 : 1 }

    func previewPanel(_ panel: QLPreviewPanel!, previewItemAt index: Int) -> QLPreviewItem! { url as NSURL? }
}

final class SearchWindow: NSWindow {
    var quickLook: QuickLookSource?

    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }

    override func acceptsPreviewPanelControl(_ panel: QLPreviewPanel!) -> Bool { true }

    override func beginPreviewPanelControl(_ panel: QLPreviewPanel!) {
        panel.dataSource = quickLook
        panel.delegate = quickLook
    }

    override func endPreviewPanelControl(_ panel: QLPreviewPanel!) {
        panel.dataSource = nil
        panel.delegate = nil
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    static var shared: AppDelegate?
    let opts = LaunchOptions.parse(CommandLine.arguments)
    var window: SearchWindow!
    var model: SearchModel!
    var statusItem: NSStatusItem!
    let quickLook = QuickLookSource()
    var monitor: Any?
    var hotKey: EventHotKeyRef?
    var lastWasNav = false
    var snapshotDone = false

    func applicationDidFinishLaunching(_ n: Notification) {
        AppDelegate.shared = self
        model = SearchModel(opts: opts)
        buildWindow()
        buildMenus()
        installKeyMonitor()
        registerHotKey()
        model.onResults = { [weak self] in self?.maybeSnapshot() }
        model.load()
        show()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ s: NSApplication) -> Bool { false }

    private func buildWindow() {
        let w = SearchWindow(contentRect: NSRect(x: 0, y: 0, width: 800, height: 520),
                             styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
        w.title = "FlashFind"
        w.titleVisibility = .hidden
        w.titlebarAppearsTransparent = true
        w.isMovableByWindowBackground = true
        w.isReleasedWhenClosed = false
        w.level = .floating
        w.collectionBehavior = [.moveToActiveSpace, .fullScreenAuxiliary]
        w.standardWindowButton(.miniaturizeButton)?.isHidden = true
        w.standardWindowButton(.zoomButton)?.isHidden = true
        w.backgroundColor = .windowBackgroundColor
        w.contentView = NSHostingView(rootView: ContentView(model: model))
        w.quickLook = quickLook
        w.center()
        window = w
    }

    private func buildMenus() {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        statusItem.button?.image = NSImage(systemSymbolName: "bolt.horizontal.circle", accessibilityDescription: "FlashFind")
        let m = NSMenu()
        m.addItem(item("Show FlashFind   ⌃⌥Space", #selector(showAction)))
        m.addItem(.separator())
        m.addItem(item("Code projects", #selector(codeMode)))
        m.addItem(item("Whole disk", #selector(diskMode)))
        m.addItem(.separator())
        m.addItem(NSMenuItem(title: "Quit FlashFind", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"))
        statusItem.menu = m

        // Accessory apps still need an Edit menu for ⌘X/⌘V/⌘A in text fields.
        let main = NSMenu()
        let appItem = NSMenuItem()
        let appMenu = NSMenu()
        appMenu.addItem(NSMenuItem(title: "Quit FlashFind", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"))
        appItem.submenu = appMenu
        main.addItem(appItem)
        let editItem = NSMenuItem()
        let edit = NSMenu(title: "Edit")
        edit.addItem(NSMenuItem(title: "Undo", action: Selector(("undo:")), keyEquivalent: "z"))
        edit.addItem(NSMenuItem(title: "Redo", action: Selector(("redo:")), keyEquivalent: "Z"))
        edit.addItem(NSMenuItem(title: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x"))
        edit.addItem(NSMenuItem(title: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c"))
        edit.addItem(NSMenuItem(title: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v"))
        edit.addItem(NSMenuItem(title: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a"))
        editItem.submenu = edit
        main.addItem(editItem)
        NSApp.mainMenu = main
    }

    private func item(_ title: String, _ sel: Selector) -> NSMenuItem {
        let i = NSMenuItem(title: title, action: sel, keyEquivalent: "")
        i.target = self
        return i
    }

    @objc func showAction() { show() }
    @objc func codeMode() { model.mode = .code; show() }
    @objc func diskMode() { model.mode = .disk; show() }

    func show() {
        captureFrontmost()
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }

    /// Before we take focus: which app (and window title, if readable) was in front.
    /// Window titles of other apps need Screen Recording permission; we only
    /// read them if that was already granted (no prompt).
    private func captureFrontmost() {
        guard let app = NSWorkspace.shared.frontmostApplication,
              app.processIdentifier != ProcessInfo.processInfo.processIdentifier else { return }
        var title: String?
        if CGPreflightScreenCaptureAccess(),
           let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] {
            title = list.first(where: { ($0[kCGWindowOwnerPID as String] as? Int32) == app.processIdentifier && ($0[kCGWindowLayer as String] as? Int) == 0 })?[kCGWindowName as String] as? String
        }
        model.setContext(pid: app.processIdentifier, bundle: app.bundleIdentifier ?? app.localizedName ?? "", title: title)
    }

    func toggle() {
        if window.isVisible && window.isKeyWindow { window.orderOut(nil) } else { show() }
    }

    // MARK: keyboard

    private var quickLookVisible: Bool {
        QLPreviewPanel.sharedPreviewPanelExists() && QLPreviewPanel.shared().isVisible
    }

    private func toggleQuickLook() {
        guard let h = model.selectedHit else { return }
        quickLook.url = URL(fileURLWithPath: h.path)
        guard let panel = QLPreviewPanel.shared() else { return }
        if quickLookVisible {
            panel.orderOut(nil)
        } else {
            panel.makeKeyAndOrderFront(nil)
            panel.reloadData()
        }
    }

    private func syncQuickLook() {
        guard quickLookVisible, let h = model.selectedHit else { return }
        quickLook.url = URL(fileURLWithPath: h.path)
        QLPreviewPanel.shared().reloadData()
    }

    private func textIsSelected() -> Bool {
        ((window.firstResponder as? NSTextView)?.selectedRange().length ?? 0) > 0
    }

    private func installKeyMonitor() {
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] e in
            guard let self else { return e }
            return self.handle(e)
        }
    }

    private func handle(_ e: NSEvent) -> NSEvent? {
        guard e.window === window || quickLookVisible else { return e }
        let mods = e.modifierFlags.intersection([.command, .option, .control, .shift])
        switch Int(e.keyCode) {
        case kVK_DownArrow:
            model.move(1); syncQuickLook(); lastWasNav = true; return nil
        case kVK_UpArrow:
            model.move(-1); syncQuickLook(); lastWasNav = true; return nil
        case kVK_Return, kVK_ANSI_KeypadEnter:
            if mods.contains(.command) { model.reveal() } else if mods.contains(.option) { model.openInCursor() } else { model.open() }
            return nil
        case kVK_Escape:
            if quickLookVisible { QLPreviewPanel.shared().orderOut(nil); return nil }
            if model.query.isEmpty { window.orderOut(nil) } else { model.query = "" }
            return nil
        case kVK_Space where mods.isEmpty && (lastWasNav || quickLookVisible) && model.selectedHit != nil:
            toggleQuickLook(); return nil
        case kVK_ANSI_Y where mods == .command:
            toggleQuickLook(); return nil
        case kVK_ANSI_C where mods == .command && !textIsSelected() && model.selectedHit != nil:
            model.copyPath(); return nil
        case kVK_ANSI_1 where mods == .command:
            model.mode = .code; return nil
        case kVK_ANSI_2 where mods == .command:
            model.mode = .disk; return nil
        case kVK_ANSI_W where mods == .command:
            window.orderOut(nil); return nil
        default:
            break
        }
        lastWasNav = false
        return e
    }

    private func registerHotKey() {
        var spec = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
        InstallEventHandler(GetApplicationEventTarget(), { _, _, _ -> OSStatus in
            DispatchQueue.main.async { AppDelegate.shared?.toggle() }
            return noErr
        }, 1, &spec, nil, nil)
        let id = EventHotKeyID(signature: OSType(0x4646_4E44), id: 1) // 'FFND'
        RegisterEventHotKey(UInt32(kVK_Space), UInt32(controlKey | optionKey), id, GetApplicationEventTarget(), 0, &hotKey)
    }

    // MARK: scripted verification (--snapshot / --window-id-file / --quit-after)

    private func maybeSnapshot() {
        guard !snapshotDone, opts.snapshot != nil || opts.windowIdFile != nil || opts.quitAfter != nil else { return }
        if opts.query != nil && model.rows.isEmpty && model.hint == nil { return }
        snapshotDone = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.8) { [self] in
            if let f = opts.windowIdFile {
                try? "\(window.windowNumber)\n\(model.status)\n".write(toFile: f, atomically: true, encoding: .utf8)
            }
            if let p = opts.snapshot, let view = window.contentView,
               let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) {
                view.cacheDisplay(in: view.bounds, to: rep)
                try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: p))
            }
            if let q = opts.quitAfter {
                DispatchQueue.main.asyncAfter(deadline: .now() + q) { NSApp.terminate(nil) }
            }
        }
    }
}

@main
enum Main {
    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.setActivationPolicy(.accessory)
        app.run()
    }
}
