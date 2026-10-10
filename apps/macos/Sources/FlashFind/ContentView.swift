import SwiftUI

struct ContentView: View {
    @ObservedObject var model: SearchModel
    @FocusState private var focused: Bool

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Image(systemName: "magnifyingglass")
                    .font(.system(size: 19, weight: .medium))
                    .foregroundStyle(.secondary)
                TextField(model.mode == .code ? "Search all code projects   repo:  lang:  dirty:" : "Search the whole disk", text: $model.query)
                    .textFieldStyle(.plain)
                    .font(.system(size: 22))
                    .focused($focused)
                    .onChange(of: model.query) { _, _ in model.search() }
                Picker("", selection: $model.mode) {
                    Text("Code").tag(Mode.code)
                    Text("Disk").tag(Mode.disk)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .frame(width: 120)
                .help("⌘1 code projects · ⌘2 whole disk")
                .onChange(of: model.mode) { _, _ in model.modeChanged() }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 12)
            Divider()
            ZStack {
                if let hint = model.hint {
                    Text(hint)
                        .multilineTextAlignment(.center)
                        .foregroundStyle(.secondary)
                        .padding(30)
                } else if model.rows.isEmpty {
                    Text(model.query.isEmpty ? (model.ready ? "Type to search every git repo under your home folder.\nTry  Cargo.toml   repo:flashfind readme   lang:swift view   main.rs:42\nFiles you open, edit and commit show up here next time." : "Opening the project index…") : "No matches")
                        .multilineTextAlignment(.center)
                        .foregroundStyle(.secondary)
                } else {
                    results
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            Divider()
            HStack(spacing: 12) {
                if let name = model.contextName, model.mode == .code {
                    Label(name, systemImage: "scope")
                        .lineLimit(1)
                        .help("Current project (\(model.contextHow)): its files, uncommitted and recently committed ones rank first")
                }
                Text(model.status).monospacedDigit().lineLimit(1).truncationMode(.tail).layoutPriority(1)
                Spacer(minLength: 8)
                Text(model.marked.isEmpty ? "↩ open · ⌥↩ Cursor · ⌘↩ reveal · ⌘C path · ⌘⇧C rel · ⌥⌘C @path · ⇥ mark" : "\(model.marked.count) marked · ⌘C / ⌘⇧C / ⌥⌘C copy all · ↩ open all · esc unmark")
                    .lineLimit(1)
                    .help("⌥↩ opens in Cursor at file:LINE when the query ends with :LINE. ⇥ marks rows for multi-copy. ⌘Y or Space (after ↑/↓) previews. Esc clears, then hides. ⌘1/⌘2 switch mode. ⌃⌥Space toggles from anywhere.")
            }
            .font(.system(size: 11))
            .foregroundStyle(.secondary)
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
        }
        .frame(minWidth: 640, minHeight: 360)
        .background(Color(nsColor: .windowBackgroundColor))
        .onAppear { focused = true }
    }

    private var results: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if model.suggesting {
                        HStack(spacing: 6) {
                            Image(systemName: "sparkles").foregroundStyle(.yellow)
                            Text(model.contextName.map { "Suggested · working in \($0)" } ?? "Suggested · recent and frequent files")
                                .font(.system(size: 12, weight: .semibold))
                            Text("type to search everything").font(.system(size: 11)).foregroundStyle(.secondary)
                        }
                        .padding(.horizontal, 12)
                        .padding(.top, 6)
                    }
                    ForEach(model.rows) { row in
                        RowView(row: row, selected: row.hitIndex == model.selected, marked: row.hitIndex.map { model.marked.contains($0) } ?? false)
                            .id(row.id)
                            .contentShape(Rectangle())
                            .onTapGesture(count: 2) {
                                if let i = row.hitIndex { model.selected = i; model.open() }
                            }
                            .onTapGesture {
                                if let i = row.hitIndex { model.selected = i }
                            }
                    }
                }
                .padding(.vertical, 4)
            }
            .onChange(of: model.selected) { _, new in
                if let row = model.rows.first(where: { $0.hitIndex == new }) {
                    proxy.scrollTo(row.id)
                }
            }
        }
    }
}

struct RowView: View {
    let row: Row
    let selected: Bool
    var marked = false

    var body: some View {
        if row.header {
            HStack(spacing: 6) {
                Image(systemName: "shippingbox.fill").foregroundStyle(.orange)
                Text(row.title).font(.system(size: 12, weight: .semibold))
                Text(row.subtitle).font(.system(size: 11)).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
            }
            .padding(.horizontal, 12)
            .padding(.top, 8)
            .padding(.bottom, 2)
        } else {
            let name = (row.title as NSString).lastPathComponent
            let dir = (row.title as NSString).deletingLastPathComponent
            HStack(spacing: 8) {
                Image(systemName: marked ? "checkmark.circle.fill" : (row.isDir ? "folder" : "doc.text"))
                    .frame(width: 16)
                    .foregroundStyle(selected ? .white : (marked ? Color.accentColor : .secondary))
                Text(name).font(.system(size: 13, weight: .medium)).lineLimit(1)
                Text(dir).font(.system(size: 12)).foregroundStyle(selected ? .white.opacity(0.8) : .secondary).lineLimit(1).truncationMode(.head)
                Spacer(minLength: 0)
                if !row.why.isEmpty {
                    Text(row.why)
                        .font(.system(size: 10, weight: .medium))
                        .foregroundStyle(selected ? .white.opacity(0.9) : .secondary)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 1)
                        .background(Capsule().fill(selected ? Color.white.opacity(0.18) : Color.secondary.opacity(0.12)))
                        .lineLimit(1)
                }
            }
            .foregroundStyle(selected ? .white : .primary)
            .padding(.horizontal, 10)
            .padding(.vertical, 4)
            .background(RoundedRectangle(cornerRadius: 5).fill(selected ? Color.accentColor : Color.clear))
            .padding(.horizontal, 8)
        }
    }
}
