// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import Observation

/// The Deleted Files window: what each linked folder's trash holds, the restores asked for in it,
/// and the restore of the selected item.
@MainActor
@Observable
public final class DeletedFilesModel {
    public enum Tab: Hashable, Sendable {
        case deletedItems
        case restores
    }

    public private(set) var content: LoadState<[FolderHistory]> = .loading
    public var scope: FolderScope = .all
    public var tab: Tab = .deletedItems
    public var search = ""
    public var selection: DeletedEntry.ID?
    public private(set) var isRestoring = false
    public private(set) var restoreError: SessionError?
    private var kinds: [String: NodeKind] = [:]
    private let app: AppModel
    private let clock: HistoryClock

    public init(app: AppModel, clock: HistoryClock = HistoryClock()) {
        self.app = app
        self.clock = clock
    }

    /// The linked folders the scope pop-up offers after All folders.
    public var folders: [LinkedFolder] { app.state.folders }

    /// True when the load failed while the session can't reach the server.
    public var isOffline: Bool {
        if case .failed = content { app.state.status == .offline } else { false }
    }

    /// The folders in scope, in the order of the linked folders.
    public var scopedFolders: [FolderHistory] {
        guard case .loaded(let histories) = content else { return [] }
        return histories.filter { history in
            switch scope {
            case .all: true
            case .folder(let spaceID): history.folder.spaceId == spaceID
            }
        }
    }

    /// The deleted items in scope that match the search, folders first, each group newest deletion first.
    public var sections: [DeletedSection] {
        let query = search.trimmingCharacters(in: .whitespaces)
        let entries = scopedFolders.flatMap { history in
            history.trash.items.map { entry(for: $0, in: history) }
        }
        .filter { query.isEmpty || $0.path.localizedStandardContains(query) }
        .sorted { ($0.item.deletedAtMs ?? .min, $1.path) > ($1.item.deletedAtMs ?? .min, $0.path) }
        return [DeletedSection.Kind.folders, .files].compactMap { kind in
            let matching = entries.filter { ($0.item.kind == .folder) == (kind == .folders) }
            return matching.isEmpty ? nil : DeletedSection(kind: kind, entries: matching)
        }
    }

    public var selectedEntry: DeletedEntry? {
        guard let selection else { return nil }
        return sections.lazy.flatMap(\.entries).first { $0.id == selection }
    }

    /// The restores asked for in each folder in scope: the waiting ones first, then the ended ones, latest first.
    public var restoreSections: [RestoreSection] {
        scopedFolders.compactMap { history in
            guard !history.requests.isEmpty else { return nil }
            let waiting = history.requests.filter(\.state.isPending)
            let ended = history.requests.filter { !$0.state.isPending }.reversed()
            return RestoreSection(folder: history.folder, requests: waiting + ended, kinds: kinds)
        }
    }

    public var pendingRestoreCount: Int {
        scopedFolders.reduce(0) { $0 + $1.requests.count(where: \.state.isPending) }
    }

    public var hasPendingRestores: Bool { pendingRestoreCount > 0 }

    /// Reads every linked folder's trash and restores. A load over loaded content keeps showing it until the answer.
    public func load() async {
        if case .loaded = content {} else { content = .loading }
        do {
            var histories: [FolderHistory] = []
            for folder in app.state.folders {
                let trash = try await app.trash(in: folder.url)
                let requests = try await app.restoreRequests(in: folder.url)
                histories.append(FolderHistory(folder: folder, trash: trash, requests: requests))
            }
            remember(histories)
            content = .loaded(histories)
        } catch {
            content = .failed(sessionError(error))
        }
    }

    /// Reads the restores again. A folder whose restore ended reads its trash again too, since a restored item leaves it.
    public func refreshRequests() async {
        guard case .loaded(var histories) = content else { return }
        for index in histories.indices {
            let history = histories[index]
            guard let requests = try? await app.restoreRequests(in: history.folder.url) else { continue }
            let waited = Set(history.requests.filter(\.state.isPending).map(\.id))
            if requests.contains(where: { waited.contains($0.id) && !$0.state.isPending }),
               let trash = try? await app.trash(in: history.folder.url) {
                histories[index].trash = trash
            }
            histories[index].requests = requests
        }
        remember(histories)
        content = .loaded(histories)
    }

    /// Reads the restores again every `interval` while one in scope waits.
    public func followPendingRestores(every interval: Duration = .seconds(2)) async {
        while hasPendingRestores {
            do {
                try await Task.sleep(for: interval)
            } catch {
                return
            }
            await refreshRequests()
        }
    }

    /// Catches up after the session's status changed: the trash and restores load again, keeping what's shown until the answer.
    public func sessionStatusChanged() async {
        await load()
    }

    /// Asks to bring the selected item back to where it was.
    public func restoreSelected() async {
        guard !isRestoring, let entry = selectedEntry, !entry.isRequested,
              let history = scopedFolders.first(where: { $0.folder.spaceId == entry.id.spaceID }) else { return }
        isRestoring = true
        restoreError = nil
        defer { isRestoring = false }
        do {
            _ = try await app.restore(entry.item, from: history.trash)
            await refreshRequests()
        } catch {
            restoreError = sessionError(error)
        }
    }

    /// What a restore brings back, as its row in the Restores tab names it.
    public func subject(of request: RestoreRequest) -> RestoreSubject {
        switch request.target {
        case .deleted:
            return .deletedItem
        case .version(_, _, let savedAtMs?):
            let date = Date(milliseconds: savedAtMs)
            return .version(savedAt: date, day: clock.day(of: date))
        case .version:
            return .earlierVersion
        }
    }

    private func remember(_ histories: [FolderHistory]) {
        for item in histories.flatMap(\.trash.items) {
            kinds[item.nodeId] = item.kind
        }
    }

    private func entry(for item: DeletedItem, in history: FolderHistory) -> DeletedEntry {
        let requested = history.requests.contains { request in
            request.state.isPending && request.target == .deleted(nodeId: item.nodeId)
        }
        let deletion = item.deletedAtMs.map { ms in
            let date = Date(milliseconds: ms)
            return Deletion(
                date: date,
                day: clock.day(of: date),
                device: app.deviceLabel(id: item.deletedByDeviceId, name: item.deletedByDeviceName)
            )
        }
        return DeletedEntry(folder: history.folder, item: item, deletion: deletion, isRequested: requested)
    }
}

/// One linked folder's trash and the restores asked for in it.
public struct FolderHistory: Equatable, Sendable {
    public let folder: LinkedFolder
    public var trash: Trash
    public var requests: [RestoreRequest]
}

/// When an item was deleted, and on which device when the server says.
public struct Deletion: Equatable, Sendable {
    public let date: Date
    public let day: Day
    public let device: DeviceLabel?

    public init(date: Date, day: Day, device: DeviceLabel?) {
        self.date = date
        self.day = day
        self.device = device
    }
}

/// What a restore brings back. An earlier version carries when it was saved, except for a request that doesn't say.
public enum RestoreSubject: Equatable, Sendable {
    case deletedItem
    case version(savedAt: Date, day: Day)
    case earlierVersion
}

/// A deleted item as the Deleted Files list shows it.
public struct DeletedEntry: Identifiable, Equatable, Sendable {
    public struct ID: Hashable, Sendable {
        public let spaceID: String
        public let nodeID: String
    }

    public let folder: LinkedFolder
    public let item: DeletedItem
    public let deletion: Deletion?
    /// True while a restore of the item waits to go out.
    public let isRequested: Bool

    public var id: ID { ID(spaceID: folder.spaceId, nodeID: item.nodeId) }
    /// The item's path, starting with its folder's name.
    public var path: String { folder.spaceName + "/" + item.path }
    /// Where a restore puts it back, starting with its folder's name.
    public var destination: String {
        let parent = (item.path as NSString).deletingLastPathComponent
        return parent.isEmpty ? folder.spaceName : folder.spaceName + "/" + parent
    }
}

public struct DeletedSection: Identifiable, Equatable, Sendable {
    public enum Kind: Hashable, Sendable {
        case folders
        case files
    }

    public let kind: Kind
    public let entries: [DeletedEntry]
    public var id: Kind { kind }
}

public struct RestoreSection: Identifiable, Equatable, Sendable {
    public let folder: LinkedFolder
    public let requests: [RestoreRequest]
    let kinds: [String: NodeKind]
    public var id: String { folder.spaceId }

    /// What the request brings back, when this window has seen it in the trash. A version is a file.
    public func kind(of request: RestoreRequest) -> NodeKind? {
        switch request.target {
        case .deleted(let nodeId): kinds[nodeId]
        case .version: .file
        }
    }
}
