// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import Observation

/// The Shared Links window: every link this person made, and the revoke of one.
@MainActor
@Observable
public final class SharedLinksModel {
    public private(set) var content: LoadState<[ShareLink]> = .loading
    public var showsEndedLinks = false
    public private(set) var revoking: ShareLink.ID?
    public private(set) var revokeError: SessionError?
    private var revokedHere: Set<ShareLink.ID> = []
    private let app: AppModel
    private let clock: HistoryClock
    @ObservationIgnored nonisolated(unsafe) private var creations: (any NSObjectProtocol)?

    public init(app: AppModel, clock: HistoryClock = HistoryClock()) {
        self.app = app
        self.clock = clock
        creations = NotificationCenter.default.addObserver(forName: ShareLinkModel.linkCreated, object: app, queue: nil) { [weak self] note in
            guard let link = note.userInfo?[ShareLinkModel.linkKey] as? ShareLink else { return }
            MainActor.assumeIsolated { self?.add(link) }
        }
    }

    deinit {
        if let creations { NotificationCenter.default.removeObserver(creations) }
    }

    /// The names of the linked folders, which items can be shared from.
    public var folderNames: [String] { app.state.folders.map(\.spaceName) }

    /// True when the load failed while the session can't reach the server.
    public var isOffline: Bool {
        if case .failed = content { app.state.status == .offline } else { false }
    }

    /// The links that still open, newest first. A link revoked in this window stays here, revoked.
    public var activeLinks: [SharedLink] {
        links.filter { $0.state == .active || revokedHere.contains($0.id) }.map(entry)
    }

    /// The links that expired or were revoked before, newest first, while they're shown.
    public var endedLinks: [SharedLink] {
        guard showsEndedLinks else { return [] }
        return links.filter { $0.state != .active && !revokedHere.contains($0.id) }.map(entry)
    }

    /// True when `url` is in a linked folder, or is one, so it can be shared.
    public func canShare(_ url: URL) -> Bool {
        app.folder(containing: url) != nil
    }

    /// Reads every link. A load over loaded links keeps showing them until the answer.
    public func load() async {
        if case .loaded = content {} else { content = .loading }
        do {
            content = .loaded(try await app.shareLinks())
        } catch {
            content = .failed(sessionError(error))
        }
    }

    /// Catches up after the session's status changed: the links load again, keeping what's shown until the answer.
    public func sessionStatusChanged() async {
        await load()
    }

    /// Ends `link` at once. Its row stays and shows it revoked.
    public func revoke(_ link: ShareLink) async {
        guard revoking == nil else { return }
        revoking = link.id
        revokeError = nil
        defer { revoking = nil }
        do {
            let revoked = try await app.revoke(link)
            revokedHere.insert(revoked.id)
            if case .loaded(let links) = content {
                content = .loaded(links.map { $0.id == revoked.id ? revoked : $0 })
            }
        } catch {
            revokeError = sessionError(error)
        }
    }

    private func add(_ link: ShareLink) {
        guard case .loaded(let links) = content, !links.contains(where: { $0.id == link.id }) else { return }
        content = .loaded([link] + links)
    }

    private var links: [ShareLink] {
        if case .loaded(let links) = content { links } else { [] }
    }

    private func entry(for link: ShareLink) -> SharedLink {
        let created = Date(milliseconds: link.createdAtMs)
        return SharedLink(link: link, item: item(of: link), created: created, createdDay: clock.day(of: created), end: end(of: link))
    }

    private func item(of link: ShareLink) -> SharedItem {
        let folderName = link.folder.map { path in
            let root = plainPath(URL(filePath: path, directoryHint: .isDirectory))
            return app.state.folders.first { plainPath($0.url) == root }?.spaceName ?? URL(filePath: path).lastPathComponent
        }
        guard let path = link.path else { return .gone(folder: folderName) }
        let shown = [folderName, path.isEmpty ? nil : path].compactMap(\.self).joined(separator: "/")
        return .present(path: shown, kind: link.kind ?? .file)
    }

    private func end(of link: ShareLink) -> LinkEnd {
        guard let expiresAtMs = link.expiresAtMs else { return .never }
        let expires = Date(milliseconds: expiresAtMs)
        guard link.state == .active else { return .on(expires) }
        let seconds = max(expires.timeIntervalSince(clock.now()), 0)
        let days = Int(seconds / 86_400)
        let hours = Int(seconds / 3_600)
        if days > 0 { return .remaining(.days(days)) }
        if hours > 0 { return .remaining(.hours(hours)) }
        return .remaining(.lessThanAnHour)
    }
}

/// A share link as the Shared Links list shows it.
public struct SharedLink: Identifiable, Equatable, Sendable {
    public let link: ShareLink
    public let item: SharedItem
    public let created: Date
    public let createdDay: Day
    public let end: LinkEnd

    public var id: ShareLink.ID { link.id }
}

/// What a link shares.
public enum SharedItem: Equatable, Sendable {
    /// The item's path, starting with its linked folder's name.
    case present(path: String, kind: NodeKind)
    /// The item is in the trash or gone. The linked folder it was in, when one on this Mac syncs it.
    case gone(folder: String?)
}

/// When a link stops working.
public enum LinkEnd: Equatable, Sendable {
    case never
    /// The time an active link has left.
    case remaining(TimeLeft)
    /// The day an ended link expired or would have.
    case on(Date)
}

public enum TimeLeft: Equatable, Sendable {
    case days(Int)
    case hours(Int)
    case lessThanAnHour
}
