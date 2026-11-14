// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import KoidriveModel
import SwiftUI
import UniformTypeIdentifiers

struct SharedLinksView: View {
    @Environment(AppModel.self) private var app
    @Environment(\.openWindow) private var openWindow
    @State private var links: SharedLinksModel
    @State private var confirming: SharedLink?
    @State private var drop: DropHover?
    private let loadsOnAppear: Bool

    init(app: AppModel) {
        self.init(links: SharedLinksModel(app: app))
    }

    init(links: SharedLinksModel, loadsOnAppear: Bool = true, drop: DropHover? = nil) {
        _links = State(initialValue: links)
        _drop = State(initialValue: drop)
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
        .frame(minWidth: 760, minHeight: 420)
        .overlay {
            if let drop {
                DropToShare(name: drop.name, folders: links.folderNames)
            }
        }
        .onDrop(of: [.fileURL], delegate: ShareDrop(
            hover: { drop = $0 },
            share: { url in
                if links.canShare(url) { openWindow.showShareLink(for: url) }
            }
        ))
        .navigationTitle("Shared Links")
        .task { if loadsOnAppear { await links.load() } }
        .onChange(of: app.state.status) { Task { await links.sessionStatusChanged() } }
        .sheet(item: $confirming) { entry in
            ConfirmSheet(
                title: entry.revokeTitle,
                message: entry.revokeMessage,
                action: String(localized: "Revoke"),
                confirm: {
                    confirming = nil
                    Task { await links.revoke(entry.link) }
                },
                cancel: { confirming = nil }
            )
        }
    }

    private var bar: some View {
        HistoryBar {
            Text("Links anyone can open")
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Color.text)
            Spacer(minLength: 0)
            Button("Share Link…", action: pick)
                .buttonStyle(.primary)
                .disabled(links.isOffline)
        }
    }

    @ViewBuilder
    private var content: some View {
        switch links.content {
        case .loading:
            LoadingState(title: "Loading links…")
        case .failed where links.isOffline:
            EmptyState(systemImage: "wifi.slash", title: "Can’t reach the server", message: Text("Links load when Koidrive reconnects."))
        case .failed(let error):
            EmptyState(
                systemImage: "exclamationmark.triangle",
                title: "Couldn’t load links",
                message: Text(verbatim: error.message),
                imageColor: .statusRed
            ) {
                Button("Try Again") { Task { await links.load() } }
            }
        case .loaded:
            let active = links.activeLinks
            let ended = links.endedLinks
            if active.isEmpty && ended.isEmpty {
                EmptyState(systemImage: "link", title: "No shared links", message: Text("Share a file or folder to get a link anyone can open.")) {
                    Button("Share Link…", action: pick)
                }
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 2) {
                        if !active.isEmpty {
                            GroupBand(Text("Active")) {
                                Text(verbatim: linkCount(active.count)).foregroundStyle(Color.text2)
                            }
                            ColumnTitles()
                            rows(active)
                        }
                        if !ended.isEmpty {
                            GroupBand(Text("Ended")) {
                                Text(verbatim: linkCount(ended.count)).foregroundStyle(Color.text2)
                            }
                            rows(ended)
                        }
                    }
                    .padding(12)
                }
            }
        }
    }

    private func rows(_ entries: [SharedLink]) -> some View {
        ForEach(entries) { entry in
            SharedLinkRow(entry: entry, revoking: links.revoking == entry.id) { confirming = entry }
        }
    }

    private var footer: some View {
        HistoryFooter {
            Toggle("Show ended links", isOn: $links.showsEndedLinks)
                .toggleStyle(.checkbox)
                .disabled(!isLoaded)
            Spacer(minLength: 0)
            if let error = links.revokeError {
                ErrorLine(message: String(localized: "Couldn’t revoke the link. \(error.message)"))
                    .lineLimit(1)
            } else {
                Text("A link is shown in full only when it’s made.")
                    .font(.callout)
                    .foregroundStyle(Color.text2)
            }
        }
    }

    private var isLoaded: Bool {
        if case .loaded = links.content { true } else { false }
    }

    private func pick() {
        FilePicker.shareLink.choose(in: app.state.folders) { openWindow.showShareLink(for: $0) }
    }
}

struct DropHover: Equatable {
    var name: String?
}

private struct ShareDrop: DropDelegate {
    let hover: @MainActor (DropHover?) -> Void
    let share: @MainActor (URL) -> Void

    func validateDrop(info: DropInfo) -> Bool {
        info.hasItemsConforming(to: [.fileURL])
    }

    func dropEntered(info: DropInfo) {
        hover(DropHover())
        let hover = hover
        _ = info.itemProviders(for: [.fileURL]).first?.loadObject(ofClass: URL.self) { url, _ in
            guard let name = url?.lastPathComponent else { return }
            Task { @MainActor in hover(DropHover(name: name)) }
        }
    }

    func dropExited(info: DropInfo) {
        hover(nil)
    }

    func performDrop(info: DropInfo) -> Bool {
        hover(nil)
        guard let provider = info.itemProviders(for: [.fileURL]).first else { return false }
        let share = share
        _ = provider.loadObject(ofClass: URL.self) { url, _ in
            guard let url else { return }
            Task { @MainActor in share(url) }
        }
        return true
    }
}

private struct DropToShare: View {
    let name: String?
    let folders: [String]

    var body: some View {
        VStack(spacing: 8) {
            Image(systemName: "square.and.arrow.up")
                .font(.system(size: 18))
                .foregroundStyle(Color.text)
            Group {
                if let name {
                    Text("Drop to share \(name)")
                } else {
                    Text("Drop to share")
                }
            }
            .fontWeight(.semibold)
            .foregroundStyle(Color.text)
            Text("Files and folders in \(folders.formatted(.list(type: .or)))")
                .foregroundStyle(Color.text2)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(RoundedRectangle(cornerRadius: 10).fill(Color.koi.opacity(0.08)))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color.koi, style: StrokeStyle(lineWidth: 2, dash: [5, 4])))
        .background(Color.panel, in: RoundedRectangle(cornerRadius: 10))
        .padding(8)
    }
}

private struct ColumnTitles: View {
    var body: some View {
        HStack(spacing: 10) {
            Color.clear.frame(width: 16)
            Text("Item").frame(maxWidth: .infinity, alignment: .leading)
            Text("Created").frame(width: 96, alignment: .leading)
            Text("Expires").frame(width: 104, alignment: .leading)
            Color.clear.frame(width: 24)
            Text("Status").frame(width: 76, alignment: .leading)
            Color.clear.frame(width: 84)
        }
        .font(.callout.weight(.semibold))
        .foregroundStyle(Color.text2)
        .padding(.horizontal, 12)
        .frame(height: 24)
    }
}

private struct SharedLinkRow: View {
    let entry: SharedLink
    let revoking: Bool
    let revoke: () -> Void

    private var isActive: Bool { entry.link.state == .active }

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: symbol)
                .foregroundStyle(Color.text2)
                .frame(width: 16)
            item
                .padding(.vertical, 5)
                .frame(maxWidth: .infinity, alignment: .leading)
            Text(verbatim: entry.createdText)
                .foregroundStyle(Color.text2)
                .lineLimit(1)
                .frame(width: 96, alignment: .leading)
            Text(verbatim: entry.end.text)
                .foregroundStyle(Color.text2)
                .lineLimit(1)
                .frame(width: 104, alignment: .leading)
            ZStack {
                if entry.link.hasPassword {
                    Image(systemName: "lock")
                        .foregroundStyle(Color.text2)
                        .help("Needs a password")
                        .accessibilityLabel(Text("Needs a password"))
                }
            }
            .frame(width: 24)
            state
                .frame(width: 76, alignment: .leading)
            ZStack {
                if isActive {
                    Button("Revoke…", role: .destructive, action: revoke)
                        .controlSize(.small)
                        .disabled(revoking)
                }
            }
            .frame(width: 84, alignment: .trailing)
        }
        .padding(.horizontal, 12)
        .frame(minHeight: 44)
    }

    private var symbol: String {
        switch entry.item {
        case .present(_, .folder): "folder"
        case .present: "doc.text"
        case .gone: "info.circle"
        }
    }

    @ViewBuilder
    private var item: some View {
        switch entry.item {
        case .present(let path, _) where isActive:
            PathText(path: path)
                .help(path)
        case .present(let path, _):
            Text(verbatim: path)
                .foregroundStyle(Color.text2)
                .lineLimit(1)
                .truncationMode(.middle)
                .help(path)
        case .gone(let folder):
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 0) {
                    if let folder {
                        Text(verbatim: folder + "/")
                    }
                    Text("Item no longer there")
                }
                .foregroundStyle(Color.text2)
                .lineLimit(1)
                Text("The item was moved or deleted")
                    .font(.callout)
                    .foregroundStyle(Color.text2)
            }
        }
    }

    @ViewBuilder
    private var state: some View {
        switch entry.link.state {
        case .active:
            StatusPill(title: String(localized: "Active"), mark: .dot(.statusGreen))
        case .expired:
            Text("Expired").foregroundStyle(Color.text2)
        case .revoked:
            Text("Revoked").foregroundStyle(Color.text2)
        }
    }
}

extension SharedLink {
    var revokeTitle: String {
        switch item {
        case .present(let path, _): String(localized: "Revoke the link to \((path as NSString).lastPathComponent)?")
        case .gone: String(localized: "Revoke this link?")
        }
    }

    var revokeMessage: String {
        switch item {
        case .present(_, .folder): String(localized: "It stops working at once. You can share the folder again with a new link.")
        case .present: String(localized: "It stops working at once. You can share the file again with a new link.")
        case .gone: String(localized: "It stops working at once.")
        }
    }
}

#Preview("Shared Links") {
    let app = AppModel.preview(.upToDate)
    return SharedLinksView(app: app).environment(app).frame(width: 760, height: 516)
}
