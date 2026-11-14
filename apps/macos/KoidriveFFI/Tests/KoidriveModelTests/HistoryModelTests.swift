// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import Testing

import KoidriveModel

private let utc: Calendar = {
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = TimeZone(identifier: "UTC")!
    return calendar
}()

private func at(_ text: String) -> Date {
    try! Date(text, strategy: .iso8601)
}

private func ms(_ text: String) -> Int64 {
    Int64(at(text).timeIntervalSince1970 * 1000)
}

private let halfPastMidnight = HistoryClock(now: { at("2026-11-13T00:30:00Z") }, calendar: utc)

private func deleted(
    _ id: String, _ path: String, kind: NodeKind = .file, files: UInt64 = 1, bytes: UInt64 = 1_000,
    at time: String? = nil, device: String? = nil, name: String? = nil
) -> DeletedItem {
    DeletedItem(
        nodeId: id, path: path, kind: kind, files: files, bytes: bytes, modifiedAtMs: nil,
        deletedAtMs: time.map(ms), deletedByDeviceId: device, deletedByDeviceName: name
    )
}

private func version(_ id: String, at time: String, device: String, name: String?, current: Bool = false) -> FileVersion {
    FileVersion(versionId: id, size: 1_000, savedAtMs: ms(time), deviceId: device, deviceName: name, current: current)
}

@MainActor
private func fakeCore(_ model: AppModel) throws -> FakeCore {
    try #require(model.session.fakeCore())
}

@MainActor
private func folder(_ model: AppModel, _ name: String) throws -> LinkedFolder {
    try #require(model.state.folders.first { $0.spaceName == name })
}

@MainActor
private func twoFoldersWithDeletions() throws -> AppModel {
    let model = AppModel.fake(scenario: .upToDate)
    let fake = try fakeCore(model)
    try fake.setTrash(spaceId: folder(model, "Documents").spaceId, items: [
        deleted("node-archive", "Archive", kind: .folder, files: 2, bytes: 14_200_000,
                at: "2026-11-10T14:32:00Z", device: "device-3", name: "Studio iMac"),
        deleted("node-budget", "Finance/Budget 2026.numbers", bytes: 248_000,
                at: "2026-11-13T00:10:00Z", device: "device-2", name: "Ada's MacBook Air"),
        deleted("node-notes", "Notes/Notes.md", bytes: 4_000, at: "2026-11-11T09:06:00Z", device: "device-1"),
    ])
    try fake.setTrash(spaceId: folder(model, "Photos").spaceId, items: [
        deleted("node-trip", "2026/Trip drafts", kind: .folder, files: 18, bytes: 96_000_000,
                at: "2026-11-12T23:50:00Z", device: "device-1", name: "Ada's Mac mini"),
        deleted("node-photo", "2026/Seoul/IMG_0412.heic", bytes: 3_100_000,
                at: "2026-11-12T07:55:00Z", device: "device-9", name: nil),
    ])
    return model
}

@MainActor
@Suite struct DeletedFilesModelTests {
    @Test func listsEveryFolderGroupedIntoFoldersAndFilesNewestFirst() async throws {
        let files = DeletedFilesModel(app: try twoFoldersWithDeletions(), clock: halfPastMidnight)
        #expect(files.content == .loading)
        await files.load()

        #expect(files.sections.map(\.kind) == [.folders, .files])
        #expect(files.sections[0].entries.map(\.path) == ["Photos/2026/Trip drafts", "Documents/Archive"])
        #expect(files.sections[1].entries.map(\.path) == [
            "Documents/Finance/Budget 2026.numbers", "Photos/2026/Seoul/IMG_0412.heic", "Documents/Notes/Notes.md",
        ])
        #expect(files.sections.map(\.entries.count) == [2, 3])
        #expect(files.folders.map(\.spaceName) == ["Documents", "Photos"])
    }

    @Test func theScopeAndTheSearchNarrowTheList() async throws {
        let app = try twoFoldersWithDeletions()
        let files = DeletedFilesModel(app: app, clock: halfPastMidnight)
        await files.load()

        files.scope = .folder(spaceID: try folder(app, "Photos").spaceId)
        #expect(files.sections.flatMap(\.entries).map(\.path) == ["Photos/2026/Trip drafts", "Photos/2026/Seoul/IMG_0412.heic"])

        files.search = "seoul"
        #expect(files.sections.map(\.kind) == [.files])
        #expect(files.sections.flatMap(\.entries).map(\.item.nodeId) == ["node-photo"])

        files.scope = .all
        files.search = "  notes "
        #expect(files.sections.flatMap(\.entries).map(\.item.nodeId) == ["node-notes"])

        files.search = "nothing like this"
        #expect(files.sections.isEmpty)
    }

    @Test func aDeletionSaysWhenAndOnWhichDevice() async throws {
        let app = try twoFoldersWithDeletions()
        let fake = try fakeCore(app)
        try fake.setTrash(spaceId: folder(app, "Documents").spaceId, items: [
            deleted("node-budget", "Budget.numbers", at: "2026-11-13T00:10:00Z", device: "device-2", name: "Ada's MacBook Air"),
            deleted("node-notes", "Notes.md", at: "2026-11-12T09:06:00Z", device: "device-1", name: "Ada's Mac mini"),
            deleted("node-old", "Old.md", at: "2026-11-02T09:06:00Z", device: "device-9", name: nil),
            deleted("node-undated", "Undated.md", device: "device-2", name: "Ada's MacBook Air"),
        ])
        let files = DeletedFilesModel(app: app, clock: halfPastMidnight)
        await files.load()
        files.scope = .folder(spaceID: try folder(app, "Documents").spaceId)

        let deletions = Dictionary(uniqueKeysWithValues: files.sections.flatMap(\.entries).map { ($0.item.nodeId, $0.deletion) })
        #expect(deletions["node-budget"] == Deletion(date: at("2026-11-13T00:10:00Z"), day: .today, device: .named("Ada's MacBook Air")))
        #expect(deletions["node-notes"] == Deletion(date: at("2026-11-12T09:06:00Z"), day: .yesterday, device: .thisMac))
        #expect(deletions["node-old"] == Deletion(date: at("2026-11-02T09:06:00Z"), day: .earlier, device: nil))
        #expect(deletions["node-undated"] == .some(nil))
    }

    @Test func aRestoreIsRequestedThenEndsAsRestoredAndTheItemLeaves() async throws {
        let app = try twoFoldersWithDeletions()
        let files = DeletedFilesModel(app: app, clock: halfPastMidnight)
        await files.load()
        let budget = try #require(files.sections[1].entries.first)
        #expect(budget.destination == "Documents/Finance")
        files.selection = budget.id

        await files.restoreSelected()
        #expect(files.restoreError == nil)
        #expect(files.selectedEntry?.isRequested == true)
        #expect(files.pendingRestoreCount == 1)
        #expect(files.hasPendingRestores)
        let section = try #require(files.restoreSections.first)
        #expect(section.folder.spaceName == "Documents")
        #expect(section.requests.map(\.state) == [.pending(heldByProblem: false)])
        #expect(section.requests.map(\.path) == ["Finance/Budget 2026.numbers"])

        await files.restoreSelected()
        #expect(files.restoreSections[0].requests.count == 1)

        try fakeCore(app).finishRestore(requestId: section.requests[0].id, state: .restored)
        await files.followPendingRestores(every: .milliseconds(1))
        #expect(!files.hasPendingRestores)
        #expect(files.restoreSections[0].requests.map(\.state) == [.restored])
        #expect(!files.sections.flatMap(\.entries).contains { $0.id == budget.id })
        #expect(files.selectedEntry == nil)
    }

    @Test func eachEndOfARestoreIsListedWithTheWaitingOnesFirst() async throws {
        let app = try twoFoldersWithDeletions()
        let fake = try fakeCore(app)
        let files = DeletedFilesModel(app: app, clock: halfPastMidnight)
        await files.load()
        var asked: [String: String] = [:]
        for entry in files.sections.flatMap(\.entries) {
            files.selection = entry.id
            await files.restoreSelected()
            let request = try #require(files.scopedFolders.flatMap(\.requests).first { $0.target == .deleted(nodeId: entry.item.nodeId) })
            asked[entry.item.nodeId] = request.id
        }
        #expect(files.pendingRestoreCount == 5)

        try fake.finishRestore(requestId: #require(asked["node-archive"]), state: .refused(reason: .gone))
        try fake.finishRestore(requestId: #require(asked["node-budget"]), state: .restoredAs(name: "Budget 2026 2.numbers"))
        try fake.finishRestore(requestId: #require(asked["node-trip"]), state: .tooLarge)
        try fake.finishRestore(requestId: #require(asked["node-photo"]), state: .restoredAsCopy(nodeId: "node-copy", name: "IMG_0412 2.heic"))
        await files.refreshRequests()

        #expect(files.pendingRestoreCount == 1)
        #expect(files.restoreSections.map(\.folder.spaceName) == ["Documents", "Photos"])
        #expect(files.restoreSections[0].requests.map(\.state) == [
            .pending(heldByProblem: false), .restoredAs(name: "Budget 2026 2.numbers"), .refused(reason: .gone),
        ])
        #expect(files.restoreSections[1].requests.map(\.state) == [
            .restoredAsCopy(nodeId: "node-copy", name: "IMG_0412 2.heic"), .tooLarge,
        ])
        let left = files.sections.flatMap(\.entries).map(\.item.nodeId)
        #expect(!left.contains("node-budget"))
        #expect(left.contains("node-archive") && left.contains("node-trip") && left.contains("node-photo"))

        files.scope = .folder(spaceID: try folder(app, "Photos").spaceId)
        #expect(files.pendingRestoreCount == 0)
        #expect(!files.hasPendingRestores)
        files.scope = .folder(spaceID: try folder(app, "Documents").spaceId)
        #expect(files.pendingRestoreCount == 1)
    }

    @Test func aFailedRestoreKeepsTheItemAndSaysWhy() async throws {
        let app = try twoFoldersWithDeletions()
        let files = DeletedFilesModel(app: app, clock: halfPastMidnight)
        await files.load()
        files.selection = try #require(files.sections.first?.entries.first).id
        try fakeCore(app).failNext(error: .DeletedItemNotFound)

        await files.restoreSelected()
        #expect(files.restoreError == .DeletedItemNotFound)
        #expect(!files.hasPendingRestores)
        #expect(files.selectedEntry?.isRequested == false)
    }

    @Test func aFailedLoadIsAnErrorOrOfflineAndLoadsAgainWhenTheStatusChanges() async throws {
        let app = try twoFoldersWithDeletions()
        let files = DeletedFilesModel(app: app, clock: halfPastMidnight)
        try fakeCore(app).failNext(error: .ServerUnreachable(reason: "timed out"))
        await files.load()
        #expect(files.content == .failed(.ServerUnreachable(reason: "timed out")))
        #expect(!files.isOffline)

        app.apply(.status(status: .offline))
        #expect(files.isOffline)

        app.apply(.status(status: .idle))
        await files.sessionStatusChanged()
        #expect(!files.isOffline)
        #expect(files.sections.flatMap(\.entries).count == 5)
    }

    @Test func nothingDeletedLoadsAnEmptyList() async throws {
        let files = DeletedFilesModel(app: AppModel.fake(scenario: .syncing), clock: halfPastMidnight)
        await files.load()
        #expect(files.sections.isEmpty)
        #expect(files.restoreSections.isEmpty)
        #expect(!files.hasPendingRestores)
        guard case .loaded(let histories) = files.content else {
            Issue.record("not loaded")
            return
        }
        #expect(histories.map(\.folder.spaceName) == ["Documents", "Photos"])
    }
}

@MainActor
private func reportWithVersions(_ versions: [FileVersion], clock: HistoryClock = halfPastMidnight) throws -> (AppModel, VersionHistoryModel) {
    let app = AppModel.fake(scenario: .upToDate)
    let documents = try folder(app, "Documents")
    try fakeCore(app).setVersions(spaceId: documents.spaceId, versions: FileVersions(
        path: "Reports/Launch plan.key", nodeId: "node-plan", versions: versions
    ))
    let file = documents.url.appending(path: "Reports/Launch plan.key")
    return (app, VersionHistoryModel(app: app, file: file, clock: clock))
}

private let fourVersions = [
    version("v4", at: "2026-11-13T00:05:00Z", device: "device-1", name: "Ada's Mac mini", current: true),
    version("v3", at: "2026-11-12T23:55:00Z", device: "device-2", name: "Ada's MacBook Air"),
    version("v2", at: "2026-11-12T00:00:00Z", device: "device-1", name: "Ada's Mac mini"),
    version("v1", at: "2026-11-11T23:59:59Z", device: "device-9", name: nil),
]

@MainActor
@Suite struct VersionHistoryModelTests {
    @Test func versionsAreGroupedByDayAcrossMidnight() async throws {
        let (_, history) = try reportWithVersions(fourVersions)
        #expect(history.name == "Launch plan.key")
        #expect(history.parentPath == "Documents/Reports/")
        await history.load()

        #expect(history.days.map(\.day) == [.today, .yesterday, .earlier])
        #expect(history.days.map { $0.versions.map(\.versionId) } == [["v4"], ["v3", "v2"], ["v1"]])

        let (_, earlierClock) = try reportWithVersions(
            fourVersions, clock: HistoryClock(now: { at("2026-11-12T23:59:00Z") }, calendar: utc)
        )
        await earlierClock.load()
        #expect(earlierClock.days.map { $0.versions.map(\.versionId) } == [["v4", "v3", "v2"], ["v1"]])
        #expect(earlierClock.days.map(\.day) == [.today, .yesterday])
    }

    @Test func thisMacNamesItsOwnVersions() async throws {
        let (_, history) = try reportWithVersions(fourVersions)
        await history.load()
        #expect(history.versions.map(history.device(of:)) == [.thisMac, .named("Ada's MacBook Air"), .thisMac, nil])
    }

    @Test func aRestoredVersionIsRequestedThenCurrent() async throws {
        let (app, history) = try reportWithVersions(fourVersions)
        await history.load()
        #expect(!history.canRestore)
        history.selection = "v4"
        #expect(!history.canRestore)
        history.selection = "v3"
        #expect(history.canRestore)

        await history.restoreSelected()
        guard case .requested(let asked, let requestID) = history.restore else {
            Issue.record("not requested: \(history.restore)")
            return
        }
        #expect(asked.versionId == "v3")
        #expect(history.requestedVersion?.versionId == "v3")
        #expect(!history.canRestore)

        let documents = try folder(app, "Documents")
        var restored = fourVersions.map { FileVersion(versionId: $0.versionId, size: $0.size, savedAtMs: $0.savedAtMs,
                                                      deviceId: $0.deviceId, deviceName: $0.deviceName, current: false) }
        restored.insert(version("v5", at: "2026-11-13T00:20:00Z", device: "device-1", name: "Ada's Mac mini", current: true), at: 0)
        try fakeCore(app).setVersions(spaceId: documents.spaceId, versions: FileVersions(
            path: "Reports/Launch plan.key", nodeId: "node-plan", versions: restored
        ))
        try fakeCore(app).finishRestore(requestId: requestID, state: .restored)
        await history.followRestore(every: .milliseconds(1))

        #expect(history.restore == .restored(asked, .restored))
        #expect(history.versions.map(\.versionId) == ["v5", "v4", "v3", "v2", "v1"])
        #expect(history.days.first?.versions.count == 2)
        #expect(history.selection == nil)
        history.done()
        #expect(history.restore == .idle)
    }

    @Test func aRefusedRestoreSaysWhyAndCanBeAskedAgain() async throws {
        let (app, history) = try reportWithVersions(fourVersions)
        await history.load()
        history.selection = "v1"
        await history.restoreSelected()
        guard case .requested(let version, let requestID) = history.restore else {
            Issue.record("not requested")
            return
        }
        try fakeCore(app).finishRestore(requestId: requestID, state: .refused(reason: .gone))
        await history.refreshRestore()
        #expect(history.restore == .refused(version, .refused(reason: .gone)))
        #expect(history.canRestore)

        try fakeCore(app).failNext(error: .VersionNotFound)
        await history.restoreSelected()
        #expect(history.restore == .failed(version, .VersionNotFound))
    }

    @Test func aFileTheServerDoesNotHaveIsNotSyncedYet() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let file = try folder(app, "Documents").url.appending(path: "New draft.docx")
        let history = VersionHistoryModel(app: app, file: file, clock: halfPastMidnight)
        await history.load()
        #expect(history.isNotSyncedYet)
        #expect(!history.isOffline)
        #expect(history.days.isEmpty)
    }

    @Test func aFailedLoadIsAnErrorOrOfflineAndLoadsAgainWhenTheStatusChanges() async throws {
        let (app, history) = try reportWithVersions(fourVersions)
        try fakeCore(app).failNext(error: .ServerUnreachable(reason: "timed out"))
        await history.load()
        #expect(history.content == .failed(.ServerUnreachable(reason: "timed out")))
        #expect(!history.isOffline)
        #expect(!history.isNotSyncedYet)

        app.apply(.status(status: .offline))
        #expect(history.isOffline)
        app.apply(.status(status: .idle))
        await history.sessionStatusChanged()
        #expect(history.versions.count == 4)
    }

    @Test func aFileOutsideTheLinkedFoldersIsRefused() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let file = FileManager.default.temporaryDirectory.appending(path: "Elsewhere/Notes.md")
        let history = VersionHistoryModel(app: app, file: file, clock: halfPastMidnight)
        await history.load()
        #expect(history.folder == nil)
        #expect(history.content == .failed(.PathNotInLinkedFolder(path: file.path(percentEncoded: false))))
    }
}

@MainActor
@Suite struct HistoryLaunchTests {
    @Test func onlyADebugBuildOpensAHistoryWindowAtLaunch() throws {
        let suite = "dev.koidrive.tests.\(UUID().uuidString)"
        let defaults = try #require(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        #expect(AppModel.launchWindow(in: defaults) == nil)
        defaults.set(true, forKey: "OpenDeletedFiles")
        defaults.set("Documents/Reports/Annual Report.docx", forKey: "OpenVersionHistory")
        #if DEBUG
        #expect(AppModel.launchWindow(in: defaults) == .versionHistory(path: "Documents/Reports/Annual Report.docx"))
        defaults.removeObject(forKey: "OpenVersionHistory")
        #expect(AppModel.launchWindow(in: defaults) == .deletedFiles)
        #else
        #expect(AppModel.launchWindow(in: defaults) == nil)
        #endif
    }

    @Test func aPathStartingWithAFolderNameIsAFileInThatFolder() throws {
        let app = AppModel.fake(scenario: .upToDate)
        let documents = try folder(app, "Documents")
        #expect(app.url(ofDisplayPath: "Documents/Reports/Annual Report.docx") == documents.url.appending(path: "Reports/Annual Report.docx"))
        #expect(app.url(ofDisplayPath: "Elsewhere/Notes.md") == nil)
        #expect(app.folder(containing: documents.url.appending(path: "Reports/Annual Report.docx")) == documents)
        #expect(app.folder(containing: documents.url) == documents)
        #expect(app.folder(containing: URL(filePath: documents.path + " copy/Notes.md")) == nil)
    }
}
