// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import KoidriveModel
import SwiftUI

struct DeletedFilesView: View {
    @Environment(AppModel.self) private var app
    @State private var files: DeletedFilesModel
    private let loadsOnAppear: Bool

    init(app: AppModel) {
        self.init(files: DeletedFilesModel(app: app))
    }

    init(files: DeletedFilesModel, loadsOnAppear: Bool = true) {
        _files = State(initialValue: files)
        self.loadsOnAppear = loadsOnAppear
    }

    var body: some View {
        VStack(spacing: 0) {
            bar
            content
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            footer
        }
        .background(Color.panel)
        .frame(minWidth: 720, minHeight: 420)
        .navigationTitle("Deleted Files")
        .task { if loadsOnAppear { await files.load() } }
        .task(id: files.hasPendingRestores) { await files.followPendingRestores() }
        .onChange(of: app.state.status) { Task { await files.sessionStatusChanged() } }
        .onChange(of: app.state.folders.map(\.spaceId)) { Task { await files.load() } }
    }

    private var bar: some View {
        HistoryBar {
            Picker(selection: $files.scope) {
                Label("All folders", systemImage: "folder").tag(FolderScope.all)
                Divider()
                ForEach(files.folders, id: \.spaceId) { folder in
                    Label(folder.spaceName, systemImage: "folder").tag(FolderScope.folder(spaceID: folder.spaceId))
                }
            } label: {
                Label("Folder", systemImage: "folder")
            }
            .pickerStyle(.menu)
            .labelsHidden()
            .fixedSize()
            .accessibilityIdentifier("scope")
            Picker("Show", selection: $files.tab) {
                Text("Deleted Items").tag(DeletedFilesModel.Tab.deletedItems)
                Text("Restores").tag(DeletedFilesModel.Tab.restores)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            if files.pendingRestoreCount > 0 {
                CountBadge(count: files.pendingRestoreCount, fill: .fillStrong, text: .text)
                    .accessibilityLabel(Text("\(files.pendingRestoreCount) restores waiting"))
            }
            Spacer(minLength: 0)
            if files.tab == .deletedItems {
                SearchField(text: $files.search)
            }
            Button {
                Task { await files.load() }
            } label: {
                Image(systemName: "arrow.clockwise")
                    .foregroundStyle(Color.text2)
                    .frame(width: 20, height: 20)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help("Refresh")
            .accessibilityLabel(Text("Refresh"))
        }
    }

    @ViewBuilder
    private var content: some View {
        switch files.content {
        case .loading:
            LoadingState(title: files.tab == .deletedItems ? "Loading deleted files…" : "Loading restores…")
        case .failed where files.isOffline:
            EmptyState(
                systemImage: "wifi.slash",
                title: "Can’t reach the server",
                message: Text("Deleted files load when Koidrive reconnects.")
            )
        case .failed(let error):
            EmptyState(
                systemImage: "exclamationmark.triangle",
                title: "Couldn’t load deleted files",
                message: Text(verbatim: error.message),
                imageColor: .statusRed
            ) {
                Button("Try Again") { Task { await files.load() } }
            }
        case .loaded:
            switch files.tab {
            case .deletedItems: deletedItems
            case .restores: restores
            }
        }
    }

    @ViewBuilder
    private var deletedItems: some View {
        let sections = files.sections
        if sections.isEmpty {
            emptyDeletedItems
        } else {
            HistoryList(selection: $files.selection, ids: sections.flatMap(\.entries).map(\.id)) {
                ForEach(sections) { section in
                    if section.id != sections.first?.id { GroupGap() }
                    GroupBand(Text(section.kind.title)) {
                        Text(section.entries.count.formatted()).foregroundStyle(Color.text2)
                    }
                    ForEach(section.entries) { entry in
                        DeletedItemRow(entry: entry, selected: files.selection == entry.id)
                            .id(entry.id)
                            .onTapGesture { files.selection = entry.id }
                    }
                }
            }
        }
    }

    @ViewBuilder
    private var emptyDeletedItems: some View {
        if !files.search.trimmingCharacters(in: .whitespaces).isEmpty {
            EmptyState(systemImage: "magnifyingglass", title: "No matches", message: Text("No deleted file matches “\(files.search)”."))
        } else if case .folder(let spaceID) = files.scope, let folder = files.folders.first(where: { $0.spaceId == spaceID }) {
            EmptyState(
                systemImage: "trash",
                title: "Nothing deleted in \(folder.spaceName)",
                message: Text("Files deleted in \(folder.spaceName) show up here.")
            )
        } else {
            EmptyState(
                systemImage: "trash",
                title: "No deleted files",
                message: Text("Files deleted in \(files.folders.map(\.spaceName).formatted(.list(type: .or))) show up here.")
            )
        }
    }

    @ViewBuilder
    private var restores: some View {
        let sections = files.restoreSections
        if sections.isEmpty {
            EmptyState(
                systemImage: "arrow.uturn.backward",
                title: "No restores yet",
                message: Text("When you restore a file, you can follow it here.")
            )
        } else {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    ForEach(sections) { section in
                        if section.id != sections.first?.id { GroupGap() }
                        GroupBand(Text(verbatim: section.folder.spaceName)) {
                            Text(verbatim: restoreCount(section.requests.count)).foregroundStyle(Color.text2)
                        }
                        ForEach(section.requests) { request in
                            RestoreRow(request: request, folder: section.folder, kind: section.kind(of: request), subject: files.subject(of: request))
                        }
                    }
                }
                .padding(12)
            }
        }
    }

    @ViewBuilder
    private var footer: some View {
        HistoryFooter {
            switch files.tab {
            case .restores:
                Text("A waiting restore goes out once its folder’s earlier changes sync.")
                    .font(.callout)
                    .foregroundStyle(Color.text2)
                    .frame(maxWidth: .infinity, alignment: .leading)
            case .deletedItems:
                let entry = files.selectedEntry
                Group {
                    if let error = files.restoreError {
                        ErrorLine(error)
                    } else if let entry {
                        VStack(alignment: .leading, spacing: 0) {
                            if entry.isRequested {
                                Text(verbatim: restoreGoesOut(with: entry.folder.spaceName))
                                    .foregroundStyle(Color.text)
                            } else {
                                HStack(alignment: .firstTextBaseline, spacing: 4) {
                                    Text("Restores to")
                                        .foregroundStyle(Color.text2)
                                    PathText(path: entry.destination)
                                }
                            }
                            Text("If the name is taken, Koidrive keeps both.")
                                .font(.callout)
                                .foregroundStyle(Color.text2)
                        }
                        .lineLimit(1)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                if entry?.isRequested == true {
                    Button("Show Restores") { files.tab = .restores }
                } else {
                    Button("Restore") { Task { await files.restoreSelected() } }
                        .buttonStyle(.primary)
                        .disabled(entry == nil || files.isRestoring)
                        .keyboardShortcut(.defaultAction)
                }
            }
        }
    }
}

private struct SearchField: View {
    @Binding var text: String

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 11))
                .foregroundStyle(Color.text2)
            TextField("Search Deleted Files", text: $text)
                .textFieldStyle(.plain)
        }
        .padding(.horizontal, 8)
        .frame(width: 200, height: 24)
        .background(RoundedRectangle(cornerRadius: 6).fill(Color.content))
        .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(.separator))
    }
}

private struct DeletedItemRow: View {
    let entry: DeletedEntry
    let selected: Bool

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: entry.item.kind == .folder ? "folder" : "doc.text")
                .foregroundStyle(Color.text2)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 0) {
                PathText(path: entry.path)
                if let deletion = entry.deletion {
                    Text(verbatim: deletion.text)
                        .font(.callout)
                        .foregroundStyle(Color.text2)
                        .lineLimit(1)
                }
            }
            .padding(.vertical, 5)
            .frame(maxWidth: .infinity, alignment: .leading)
            if entry.isRequested {
                StatusPill(title: String(localized: "Restore requested"), mark: .progress)
            }
            Text(verbatim: entry.item.kind == .folder ? fileCount(entry.item.files) : "")
                .foregroundStyle(Color.text2)
                .trailingColumn(64)
            Text(verbatim: byteSize(entry.item.bytes))
                .monospacedDigit()
                .foregroundStyle(Color.text2)
                .trailingColumn(70)
        }
        .padding(.horizontal, 12)
        .frame(minHeight: 44)
        .selectedRow(selected)
        .help(entry.path)
    }
}

private struct RestoreRow: View {
    let request: RestoreRequest
    let folder: LinkedFolder
    let kind: NodeKind?
    let subject: RestoreSubject

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: symbol)
                .foregroundStyle(Color.text2)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 0) {
                PathText(path: folder.spaceName + "/" + (request.path ?? String(localized: "Unknown item")))
                Text(verbatim: subject.text)
                    .font(.callout)
                    .foregroundStyle(Color.text2)
            }
            .padding(.vertical, 5)
            .frame(maxWidth: .infinity, alignment: .leading)
            HStack(spacing: 6) {
                outcomeMark
                Text(verbatim: request.state.text(folder: folder.spaceName))
                    .foregroundStyle(request.state.isPending ? Color.text2 : Color.text)
                    .lineLimit(1)
                    .help(request.state.text(folder: folder.spaceName))
            }
            .frame(width: 380, alignment: .leading)
        }
        .padding(.horizontal, 12)
        .frame(minHeight: 44)
    }

    private var symbol: String {
        switch request.target {
        case .version: "clock.arrow.circlepath"
        case .deleted: kind == .folder ? "folder" : "doc.text"
        }
    }

    @ViewBuilder
    private var outcomeMark: some View {
        switch request.state {
        case .pending:
            ProgressArc()
                .frame(width: 8, height: 8)
                .frame(width: 14)
        case .restored, .restoredAs:
            Image(systemName: "checkmark")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Color.statusGreen)
                .frame(width: 14)
        case .restoredAsCopy:
            Image(systemName: "info.circle")
                .font(.system(size: 12))
                .foregroundStyle(Color.statusYellow)
                .frame(width: 14)
        case .refused, .tooLarge:
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 12))
                .foregroundStyle(Color.statusRed)
                .frame(width: 14)
        }
    }
}

extension DeletedSection.Kind {
    var title: LocalizedStringKey {
        switch self {
        case .folders: "Folders"
        case .files: "Files"
        }
    }
}

#Preview("Deleted Files") {
    let app = AppModel.preview(.upToDate)
    return DeletedFilesView(app: app).environment(app).frame(width: 880, height: 600)
}
