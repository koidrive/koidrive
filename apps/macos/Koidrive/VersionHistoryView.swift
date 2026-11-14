// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import KoidriveModel
import SwiftUI

struct VersionHistoryView: View {
    @Environment(AppModel.self) private var app
    @State private var history: VersionHistoryModel
    private let loadsOnAppear: Bool

    init(app: AppModel, file: URL) {
        self.init(history: VersionHistoryModel(app: app, file: file))
    }

    init(history: VersionHistoryModel, loadsOnAppear: Bool = true) {
        _history = State(initialValue: history)
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
        .frame(minWidth: 560, minHeight: 360)
        .navigationTitle("Version History")
        .task { if loadsOnAppear { await history.load() } }
        .task(id: history.requestedVersion) { await history.followRestore() }
        .onChange(of: app.state.status) { Task { await history.sessionStatusChanged() } }
    }

    private var bar: some View {
        HistoryBar {
            Image(systemName: "doc.text")
                .font(.system(size: 15))
                .foregroundStyle(Color.text2)
                .frame(width: 20)
            Text(verbatim: history.name)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Color.text)
                .lineLimit(1)
                .layoutPriority(1)
            Text(verbatim: history.parentPath)
                .foregroundStyle(Color.text2)
                .lineLimit(1)
                .truncationMode(.head)
            Spacer(minLength: 0)
            if history.versions.count > 1 {
                Text(verbatim: versionCount(history.versions.count))
                    .foregroundStyle(Color.text2)
            }
        }
    }

    @ViewBuilder
    private var content: some View {
        switch history.content {
        case .loading:
            LoadingState(title: "Loading versions…")
        case .failed where history.isNotSyncedYet:
            EmptyState(systemImage: "clock", title: "Not synced yet", message: Text("Its versions show once it’s up to date."))
        case .failed where history.isOffline:
            EmptyState(systemImage: "wifi.slash", title: "Can’t reach the server", message: Text("Versions load when Koidrive reconnects."))
        case .failed(let error):
            EmptyState(
                systemImage: "exclamationmark.triangle",
                title: "Couldn’t load versions",
                message: Text(verbatim: error.message),
                imageColor: .statusRed
            ) {
                Button("Try Again") { Task { await history.load() } }
            }
        case .loaded where history.versions.count < 2:
            EmptyState(
                systemImage: "clock.arrow.circlepath",
                title: "No earlier versions",
                message: Text("Each synced change keeps a version here.")
            )
        case .loaded:
            let days = history.days
            HistoryList(selection: $history.selection, ids: history.versions.map(\.id)) {
                ForEach(days) { day in
                    if day.id != days.first?.id { GroupGap() }
                    GroupBand(Text(verbatim: day.day.title)) {
                        Text(day.versions.count.formatted()).foregroundStyle(Color.text2)
                    }
                    ForEach(day.versions) { version in
                        VersionRow(
                            time: day.day.time(of: Date(milliseconds: version.savedAtMs)),
                            device: history.device(of: version).title,
                            version: version,
                            requested: history.requestedVersion == version,
                            selected: history.selection == version.id
                        )
                        .id(version.id)
                        .onTapGesture { history.selection = version.id }
                    }
                }
            }
        }
    }

    @ViewBuilder
    private var footer: some View {
        HistoryFooter {
            switch history.restore {
            case .idle, .sending:
                Text("Restoring makes it the current version on every device.")
                    .font(.callout)
                    .foregroundStyle(Color.text2)
                    .frame(maxWidth: .infinity, alignment: .leading)
                restoreButton
            case .requested:
                Text(verbatim: restoreGoesOut(with: history.folder?.spaceName ?? history.name))
                    .foregroundStyle(Color.text)
                    .frame(maxWidth: .infinity, alignment: .leading)
                restoreButton
            case .restored(let version, let state):
                Label {
                    Text(verbatim: restoredText(version, state))
                        .foregroundStyle(Color.text)
                } icon: {
                    Image(systemName: "checkmark")
                        .foregroundStyle(Color.statusGreen)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                Button("Done") { history.done() }
                    .buttonStyle(.primary)
                    .keyboardShortcut(.defaultAction)
            case .refused(_, let state):
                ErrorLine(message: state.versionRefusal)
                    .frame(maxWidth: .infinity, alignment: .leading)
                restoreButton
            case .failed(_, let error):
                ErrorLine(message: String(localized: "Couldn’t restore. \(error.message)"))
                    .frame(maxWidth: .infinity, alignment: .leading)
                restoreButton
            }
        }
        .lineLimit(1)
    }

    private var restoreButton: some View {
        Button("Restore Version") { Task { await history.restoreSelected() } }
            .buttonStyle(.primary)
            .disabled(!history.canRestore)
            .keyboardShortcut(.defaultAction)
    }

    private func restoredText(_ version: FileVersion, _ state: RestoreState) -> String {
        if case .restoredAsCopy(_, let name) = state {
            return String(localized: "Restored as a copy, \(name). Another device changed the file.")
        }
        let time = history.day(of: version).time(of: Date(milliseconds: version.savedAtMs))
        return String(localized: "Restored. The \(time) version is current on every device.")
    }
}

private struct VersionRow: View {
    let time: String
    let device: String
    let version: FileVersion
    let requested: Bool
    let selected: Bool

    var body: some View {
        HStack(spacing: 10) {
            Text(verbatim: time)
                .fontWeight(.medium)
                .foregroundStyle(Color.text)
                .frame(width: 120, alignment: .leading)
            Text(verbatim: device)
                .foregroundStyle(Color.text2)
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
            if version.current {
                StatusPill(title: String(localized: "Current"), mark: .dot(.statusGreen))
            }
            if requested {
                StatusPill(title: String(localized: "Restore requested"), mark: .progress)
            }
            Text(verbatim: byteSize(version.size))
                .monospacedDigit()
                .foregroundStyle(Color.text2)
                .trailingColumn(70)
        }
        .padding(.horizontal, 12)
        .frame(height: 44)
        .selectedRow(selected)
    }
}
