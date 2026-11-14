// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import Observation

/// The Version History window of one file: its versions grouped by day, and the restore of the selected version.
@MainActor
@Observable
public final class VersionHistoryModel {
    public enum Restore: Equatable, Sendable {
        case idle
        case sending(FileVersion)
        /// Asked for; it goes out with the folder's next sync.
        case requested(FileVersion, requestID: String)
        /// Ended as `.restored`, `.restoredAs` or `.restoredAsCopy`.
        case restored(FileVersion, RestoreState)
        /// Ended as `.refused` or `.tooLarge`.
        case refused(FileVersion, RestoreState)
        case failed(FileVersion, SessionError)
    }

    public let file: URL
    public private(set) var content: LoadState<FileVersions> = .loading
    public var selection: FileVersion.ID?
    public private(set) var restore: Restore = .idle
    private var loads = 0
    private let app: AppModel
    private let clock: HistoryClock

    public init(app: AppModel, file: URL, clock: HistoryClock = HistoryClock()) {
        self.app = app
        self.file = file
        self.clock = clock
    }

    public var name: String { file.lastPathComponent }

    /// The linked folder that holds the file.
    public var folder: LinkedFolder? { app.folder(containing: file) }

    /// The file's folder, starting with its linked folder's name and ending with a slash.
    public var parentPath: String {
        let parent = file.deletingLastPathComponent()
        guard let folder else { return plainPath(parent) + "/" }
        let below = plainPath(parent).dropFirst(plainPath(folder.url).count)
        return folder.spaceName + below + "/"
    }

    /// True when the load failed while the session can't reach the server.
    public var isOffline: Bool {
        if case .failed = content { app.state.status == .offline } else { false }
    }

    /// True when the server has nothing at the file's path, as for a file that hasn't synced yet.
    public var isNotSyncedYet: Bool { content == .failed(.NothingAtPath) }

    public var versions: [FileVersion] {
        if case .loaded(let file) = content { file.versions } else { [] }
    }

    /// The versions, newest first, under Today, Yesterday and Earlier.
    public var days: [VersionDay] {
        let grouped = Dictionary(grouping: versions) { clock.day(of: Date(milliseconds: $0.savedAtMs)) }
        return [Day.today, .yesterday, .earlier].compactMap { day in
            grouped[day].map { VersionDay(day: day, versions: $0) }
        }
    }

    public func day(of version: FileVersion) -> Day {
        clock.day(of: Date(milliseconds: version.savedAtMs))
    }

    public func device(of version: FileVersion) -> DeviceLabel? {
        app.deviceLabel(id: version.deviceId, name: version.deviceName)
    }

    public var selectedVersion: FileVersion? {
        guard let selection else { return nil }
        return versions.first { $0.id == selection }
    }

    /// The version whose restore waits to go out.
    public var requestedVersion: FileVersion? {
        if case .requested(let version, _) = restore { version } else { nil }
    }

    public var canRestore: Bool {
        guard let version = selectedVersion, !version.current else { return false }
        switch restore {
        case .sending, .requested: return false
        case .idle, .restored, .refused, .failed: return true
        }
    }

    public func load() async {
        if case .loaded = content {} else { content = .loading }
        loads += 1
        let load = loads
        let answer: LoadState<FileVersions>
        do {
            answer = .loaded(try await app.versions(of: file))
        } catch {
            answer = .failed(sessionError(error))
        }
        if load == loads { content = answer }
    }

    /// Asks to make the selected version the file's content again.
    public func restoreSelected() async {
        guard canRestore, let version = selectedVersion else { return }
        restore = .sending(version)
        do {
            let asked = try await app.restore(version, of: file)
            restore = .requested(version, requestID: asked.requestId)
        } catch {
            restore = .failed(version, sessionError(error))
        }
    }

    /// Reads how the requested restore went. Once it ended as restored, the versions load again.
    public func refreshRestore() async {
        guard case .requested(let version, let id) = restore, let folder,
              let requests = try? await app.restoreRequests(in: folder.url),
              case .requested(_, id) = restore,
              let request = requests.first(where: { $0.id == id }) else { return }
        switch request.state {
        case .pending:
            return
        case .restored, .restoredAs, .restoredAsCopy:
            restore = .restored(version, request.state)
            selection = nil
            await load()
        case .refused, .tooLarge:
            restore = .refused(version, request.state)
        }
    }

    /// Reads how the requested restore went every `interval` until it ends.
    public func followRestore(every interval: Duration = .seconds(2)) async {
        while requestedVersion != nil {
            do {
                try await Task.sleep(for: interval)
            } catch {
                return
            }
            await refreshRestore()
        }
    }

    /// Catches up after the session's status changed: the versions load again, keeping what's shown until the answer,
    /// and a waiting restore is read again.
    public func sessionStatusChanged() async {
        await load()
        await refreshRestore()
    }

    /// Puts the window back to choosing a version after a restore ended.
    public func done() {
        restore = .idle
    }
}

public struct VersionDay: Identifiable, Equatable, Sendable {
    public let day: Day
    public let versions: [FileVersion]
    public var id: Day { day }
}
