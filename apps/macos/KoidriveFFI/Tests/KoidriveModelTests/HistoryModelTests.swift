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

    @Test func aDeletionThatSyncsLaterShowsUpWhenTheStatusChanges() async throws {
        let app = try twoFoldersWithDeletions()
        let files = DeletedFilesModel(app: app, clock: halfPastMidnight)
        await files.load()
        #expect(files.sections.flatMap(\.entries).count == 5)

        try fakeCore(app).setTrash(spaceId: folder(app, "Photos").spaceId, items: [
            deleted("node-trip", "2026/Trip drafts", kind: .folder, files: 18, bytes: 96_000_000, at: "2026-11-12T23:50:00Z"),
            deleted("node-photo", "2026/Seoul/IMG_0412.heic", at: "2026-11-12T07:55:00Z"),
            deleted("node-new", "2026/Seoul/IMG_0500.heic", at: "2026-11-13T00:25:00Z"),
        ])
        app.apply(.status(status: .syncing))
        await files.sessionStatusChanged()
        app.apply(.status(status: .idle))
        await files.sessionStatusChanged()
        #expect(files.sections[1].entries.first?.path == "Photos/2026/Seoul/IMG_0500.heic")
    }

    @Test func aSecondRestoreWhileTheFirstIsSendingAsksOnce() async throws {
        let files = DeletedFilesModel(app: try twoFoldersWithDeletions(), clock: halfPastMidnight)
        await files.load()
        files.selection = try #require(files.sections[1].entries.first).id

        async let first: Void = files.restoreSelected()
        async let second: Void = files.restoreSelected()
        _ = await (first, second)
        #expect(files.restoreSections.flatMap(\.requests).count == 1)
    }

    @Test func aVersionRestoreSaysWhenTheVersionWasSaved() async throws {
        let app = try twoFoldersWithDeletions()
        let documents = try folder(app, "Documents")
        try fakeCore(app).setVersions(spaceId: documents.spaceId, versions: FileVersions(
            path: "Reports/Launch plan.key", nodeId: "node-plan", versions: fourVersions
        ))
        let history = VersionHistoryModel(app: app, file: documents.url.appending(path: "Reports/Launch plan.key"), clock: halfPastMidnight)
        await history.load()
        history.selection = "v3"
        await history.restoreSelected()

        let files = DeletedFilesModel(app: app, clock: halfPastMidnight)
        await files.load()
        let asked = try #require(files.restoreSections.first?.requests.first)
        #expect(files.subject(of: asked) == .version(savedAt: at("2026-11-12T23:55:00Z"), day: .yesterday))

        func request(_ target: RestoreTarget) -> RestoreRequest {
            RestoreRequest(id: "r", path: "Reports/Launch plan.key", askedAtMs: 0, target: target, state: .pending(heldByProblem: false))
        }
        let older = request(.version(nodeId: "node-plan", versionId: "v1", savedAtMs: ms("2026-11-11T09:06:00Z")))
        #expect(files.subject(of: older) == .version(savedAt: at("2026-11-11T09:06:00Z"), day: .earlier))
        #expect(files.subject(of: request(.version(nodeId: "node-plan", versionId: "v1", savedAtMs: nil))) == .earlierVersion)
        #expect(files.subject(of: request(.deleted(nodeId: "node-budget"))) == .deletedItem)
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

    @Test func aVersionThatSyncsLaterShowsUpWhenTheStatusChanges() async throws {
        let (app, history) = try reportWithVersions(Array(fourVersions.dropFirst()))
        await history.load()
        #expect(history.versions.count == 3)

        try fakeCore(app).setVersions(spaceId: folder(app, "Documents").spaceId, versions: FileVersions(
            path: "Reports/Launch plan.key", nodeId: "node-plan", versions: fourVersions
        ))
        app.apply(.status(status: .syncing))
        await history.sessionStatusChanged()
        app.apply(.status(status: .idle))
        await history.sessionStatusChanged()
        #expect(history.versions.map(\.versionId) == ["v4", "v3", "v2", "v1"])
    }

    @Test func aSecondRestoreWhileTheFirstIsSendingAsksOnce() async throws {
        let (app, history) = try reportWithVersions(fourVersions)
        await history.load()
        history.selection = "v3"

        async let first: Void = history.restoreSelected()
        async let second: Void = history.restoreSelected()
        _ = await (first, second)
        let requests = try await app.restoreRequests(in: folder(app, "Documents").url)
        #expect(requests.count == 1)
        #expect(history.requestedVersion?.versionId == "v3")
    }

    @Test func aPathWhoseLinksDontResolveIsNeverSentToTheCore() async throws {
        let temporary = FileManager.default.temporaryDirectory.appending(path: "koidrive-chain-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        let real = try #require(realpath(temporary.path(percentEncoded: false), nil))
        defer { free(real) }
        let root = URL(filePath: String(cString: real), directoryHint: .isDirectory)
        for hop in 0..<40 {
            let destination = hop == 39 ? root.appending(path: "Outside/Missing").path(percentEncoded: false) : "chain\(hop + 1)"
            try FileManager.default.createSymbolicLink(atPath: root.appending(path: "chain\(hop)").path(percentEncoded: false), withDestinationPath: destination)
        }
        let app = AppModel.fake(scenario: .upToDate)
        try await app.unlinkFolder(spaceID: "space-photos")
        let photos = try await app.linkFolder(spaceID: "space-photos", path: root.path(percentEncoded: false))
        let fake = try #require(app.session.fakeCore())
        let old = FileVersion(versionId: "old", size: 6, savedAtMs: 1, deviceId: "device-1", deviceName: nil, current: false)
        try fake.setVersions(spaceId: photos.spaceId, versions: FileVersions(path: "chain0/Plan.key", nodeId: "synced", versions: [old]))
        let file = root.appending(path: "chain0/Plan.key")
        let refused = SessionError.PathNotInLinkedFolder(path: file.path(percentEncoded: false))

        let history = VersionHistoryModel(app: app, file: file, clock: halfPastMidnight)
        await history.load()
        history.selection = old.id
        await history.restoreSelected()
        #expect(history.folder == nil)
        #expect(history.content == .failed(refused))
        #expect(!history.canRestore)
        #expect(try await app.restoreRequests(in: photos.url).isEmpty)

        let share = ShareLinkModel(app: app, item: file)
        await share.create()
        #expect(share.folder == nil)
        #expect(share.step == .failed(refused))
    }

    @Test func aFileOutsideTheLinkedFoldersIsRefused() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let file = FileManager.default.temporaryDirectory.appending(path: "Elsewhere/Notes.md")
        let history = VersionHistoryModel(app: app, file: file, clock: halfPastMidnight)
        await history.load()
        let temporary = try #require(realpath(FileManager.default.temporaryDirectory.path(percentEncoded: false), nil))
        defer { free(temporary) }
        let real = String(cString: temporary) + "/Elsewhere/Notes.md"
        #expect(history.folder == nil)
        #expect(history.file.path(percentEncoded: false) == real)
        #expect(history.content == .failed(.PathNotInLinkedFolder(path: real)))
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

    @Test func aFileReachedThroughASymbolicLinkIsInItsLinkedFolder() async throws {
        let base = FileManager.default.temporaryDirectory.appending(path: "koidrive-alias-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: base) }
        let real = base.appending(path: "Real", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: real.appending(path: "Reports"), withIntermediateDirectories: true)
        let alias = base.appending(path: "Alias")
        try FileManager.default.createSymbolicLink(at: alias, withDestinationURL: real)
        let canonical = try #require(realpath(real.path(percentEncoded: false), nil))
        defer { free(canonical) }

        let app = AppModel.fake(scenario: .upToDate)
        try await app.unlinkFolder(spaceID: "space-photos")
        let photos = try await app.linkFolder(spaceID: "space-photos", path: String(cString: canonical))
        let file = alias.appending(path: "Reports/Plan.key")

        #expect(app.folder(containing: file) == photos)
        #expect(app.folder(containing: real.appending(path: "Reports/Plan.key")) == photos)
        #expect(app.folder(containing: alias) == photos)
        #expect(app.folder(containing: base.appending(path: "Elsewhere/Plan.key")) == nil)
        let history = VersionHistoryModel(app: app, file: file, clock: halfPastMidnight)
        #expect(history.folder == photos)
        #expect(history.parentPath == "Photos/Reports/")
    }

    private func linkedTemporaryFolder() throws -> (AppModel, URL, URL, LinkedFolder) {
        let base = FileManager.default.temporaryDirectory.appending(path: "koidrive-alias-\(UUID().uuidString)").resolvingSymlinksInPath()
        let root = base.appending(path: "Sync")
        try FileManager.default.createDirectory(at: root.appending(path: "Docs"), withIntermediateDirectories: true)
        let folder = LinkedFolder(spaceId: "space-sync", spaceName: "Sync", path: root.path(percentEncoded: false), state: .upToDate, activity: nil, bytesMoved: 0)
        let app = AppModel.fake(scenario: .upToDate)
        app.apply(.folders(folders: [folder]))
        return (app, base, root, folder)
    }

    @Test func aFinderAliasOfAFileIsThatFile() throws {
        let (app, base, root, folder) = try linkedTemporaryFolder()
        defer { try? FileManager.default.removeItem(at: base) }
        let target = root.appending(path: "Docs/Plan.key")
        try Data("plan".utf8).write(to: target)
        let alias = base.appending(path: "Plan alias")
        try URL.writeBookmarkData(target.bookmarkData(options: .suitableForBookmarkFile), to: alias)

        #expect(app.folder(containing: alias) == folder)
        #expect(SharedLinksModel(app: app).canShare(alias))
        #expect(ShareLinkModel(app: app, item: alias).name == "Plan.key")
        let history = VersionHistoryModel(app: app, file: alias, clock: halfPastMidnight)
        #expect(history.name == "Plan.key")
        #expect(history.parentPath == "Sync/Docs/")
    }

    @Test func chainsOfSymbolicLinksAndFinderAliasesLeadToTheOriginalItem() throws {
        let (app, base, root, folder) = try linkedTemporaryFolder()
        defer { try? FileManager.default.removeItem(at: base) }
        let target = root.appending(path: "Docs/Plan.key")
        try Data("plan".utf8).write(to: target)
        func alias(_ name: String, of item: URL) throws -> URL {
            let alias = base.appending(path: name)
            try URL.writeBookmarkData(item.bookmarkData(options: .suitableForBookmarkFile), to: alias)
            return alias
        }
        func link(_ name: String, to item: URL) throws -> URL {
            let link = base.appending(path: name)
            try FileManager.default.createSymbolicLink(at: link, withDestinationURL: item)
            return link
        }
        let linkToAlias = try link("Link to alias", to: alias("Plan alias", of: target))
        let aliasOfLink = try alias("Alias of link", of: link("Plan link", to: target))
        let folderAlias = try alias("Docs alias", of: root.appending(path: "Docs"))
        let aliasOfLinkToAlias = try alias("Alias of link to alias", of: linkToAlias)

        for path in [linkToAlias, aliasOfLink, folderAlias.appending(path: "Plan.key"), aliasOfLinkToAlias] {
            #expect(app.folder(containing: path) == folder, "\(path.lastPathComponent)")
            #expect(SharedLinksModel(app: app).canShare(path), "\(path.lastPathComponent)")
            #expect(ShareLinkModel(app: app, item: path).name == "Plan.key", "\(path.lastPathComponent)")
            let history = VersionHistoryModel(app: app, file: path, clock: halfPastMidnight)
            #expect(try Data(contentsOf: history.file) == Data("plan".utf8), "\(path.lastPathComponent)")
            #expect(history.parentPath == "Sync/Docs/", "\(path.lastPathComponent)")
        }
        #expect(app.folder(containing: folderAlias.appending(path: "New.key")) == folder)
    }

    @Test func aFinderAliasWhoseOriginalIsGoneIsInNoFolder() throws {
        let (app, base, root, _) = try linkedTemporaryFolder()
        defer { try? FileManager.default.removeItem(at: base) }
        let target = base.appending(path: "Gone.key")
        try Data("gone".utf8).write(to: target)
        let alias = root.appending(path: "Docs/Gone alias")
        try URL.writeBookmarkData(target.bookmarkData(options: .suitableForBookmarkFile), to: alias)
        try FileManager.default.removeItem(at: target)

        #expect(app.folder(containing: alias) == nil)
        #expect(!SharedLinksModel(app: app).canShare(alias))
        #expect(VersionHistoryModel(app: app, file: alias, clock: halfPastMidnight).folder == nil)
    }

    @Test func aLinkedFolderReachedThroughALinkFollowsItWhenLinkedAgain() throws {
        let (app, base, _, _) = try linkedTemporaryFolder()
        defer { try? FileManager.default.removeItem(at: base) }
        let first = base.appending(path: "First")
        let second = base.appending(path: "Second")
        try FileManager.default.createDirectory(at: first, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: second, withIntermediateDirectories: true)
        let link = base.appending(path: "Current")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: first)
        let folder = LinkedFolder(spaceId: "space-current", spaceName: "Current", path: link.path(percentEncoded: false), state: .upToDate, activity: nil, bytesMoved: 0)
        app.apply(.folders(folders: [folder]))
        #expect(app.folder(containing: first.appending(path: "Plan.key")) == folder)

        try FileManager.default.removeItem(at: link)
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: second)
        app.apply(.folders(folders: []))
        app.apply(.folders(folders: [folder]))
        #expect(app.folder(containing: second.appending(path: "Plan.key")) == folder)
        #expect(app.folder(containing: first.appending(path: "Plan.key")) == nil)
    }

    @Test func aPathBelongsToTheLinkedFolderThatHoldsItsRealFile() throws {
        let (app, base, root, folder) = try linkedTemporaryFolder()
        defer { try? FileManager.default.removeItem(at: base) }
        let outside = base.appending(path: "Outside")
        try FileManager.default.createDirectory(at: outside, withIntermediateDirectories: true)
        try Data("outside".utf8).write(to: outside.appending(path: "Plan.key"))
        try FileManager.default.createSymbolicLink(at: root.appending(path: "away"), withDestinationURL: outside)
        let shortcut = base.appending(path: "Shortcut")
        try FileManager.default.createSymbolicLink(at: shortcut, withDestinationURL: root)

        #expect(app.folder(containing: shortcut.appending(path: "Docs/New.key")) == folder)
        #expect(app.folder(containing: shortcut.appending(path: "away/Plan.key")) == nil)
        #expect(app.folder(containing: root.appending(path: "away/Plan.key")) == nil)
        #expect(!SharedLinksModel(app: app).canShare(shortcut.appending(path: "away/Plan.key")))
        #expect(app.folder(containing: URL(filePath: shortcut.path + "/../Plan.key")) == nil)

        try FileManager.default.createSymbolicLink(atPath: root.appending(path: "gone").path(percentEncoded: false), withDestinationPath: outside.appending(path: "Missing").path(percentEncoded: false))
        try FileManager.default.createSymbolicLink(atPath: root.appending(path: "up").path(percentEncoded: false), withDestinationPath: "../Outside/Missing")
        for path in ["gone/Plan.key", "up/Plan.key", "gone"] {
            #expect(app.folder(containing: shortcut.appending(path: path)) == nil, "\(path)")
            #expect(app.folder(containing: root.appending(path: path)) == nil, "\(path)")
            #expect(!SharedLinksModel(app: app).canShare(shortcut.appending(path: path)), "\(path)")
        }
        for hop in 0..<40 {
            try FileManager.default.createSymbolicLink(atPath: root.appending(path: "l\(hop)").path(percentEncoded: false), withDestinationPath: hop == 39 ? outside.appending(path: "Missing").path(percentEncoded: false) : "l\(hop + 1)")
        }
        #expect(app.folder(containing: root.appending(path: "l0/Plan.key")) == nil)
        #expect(!SharedLinksModel(app: app).canShare(root.appending(path: "l0/Plan.key")))
        #expect(VersionHistoryModel(app: app, file: root.appending(path: "l0/Plan.key"), clock: halfPastMidnight).folder == nil)
        #expect(ShareLinkModel(app: app, item: root.appending(path: "l0/Plan.key")).folder == nil)
        #expect(app.folder(containing: root.appending(path: "l30/Plan.key")) == nil)
        try FileManager.default.createSymbolicLink(atPath: root.appending(path: "later").path(percentEncoded: false), withDestinationPath: "Docs/Later")
        #expect(VersionHistoryModel(app: app, file: shortcut.appending(path: "later/Plan.key"), clock: halfPastMidnight).parentPath == "Sync/Docs/Later/")
    }

    @Test func aFileReachedThroughALinkFromAnotherLinkedFolderUsesTheFolderThatHoldsIt() async throws {
        let temporary = FileManager.default.temporaryDirectory.appending(path: "koidrive-bridge-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        let real = try #require(realpath(temporary.path(percentEncoded: false), nil))
        defer { free(real) }
        let base = URL(filePath: String(cString: real), directoryHint: .isDirectory)
        let first = base.appending(path: "Documents")
        let second = base.appending(path: "Photos")
        try FileManager.default.createDirectory(at: first, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: second, withIntermediateDirectories: true)
        try Data("plan".utf8).write(to: second.appending(path: "Plan.key"))
        try FileManager.default.createSymbolicLink(at: first.appending(path: "bridge"), withDestinationURL: second)

        let app = AppModel.fake(scenario: .upToDate)
        try await app.unlinkFolder(spaceID: "space-documents")
        try await app.unlinkFolder(spaceID: "space-photos")
        _ = try await app.linkFolder(spaceID: "space-documents", path: first.path(percentEncoded: false))
        let photos = try await app.linkFolder(spaceID: "space-photos", path: second.path(percentEncoded: false))
        let old = FileVersion(versionId: "old", size: 4, savedAtMs: 1, deviceId: "device-1", deviceName: nil, current: false)
        let fake = try #require(app.session.fakeCore())
        try fake.setVersions(spaceId: photos.spaceId, versions: FileVersions(path: "Plan.key", nodeId: "node-plan", versions: [old]))

        let history = VersionHistoryModel(app: app, file: first.appending(path: "bridge/Plan.key"), clock: halfPastMidnight)
        #expect(history.folder == photos)
        #expect(history.parentPath == "Photos/")
        await history.load()
        #expect(history.versions == [old])
        history.selection = old.id
        await history.restoreSelected()
        guard case .requested(_, let requestID) = history.restore else {
            Issue.record("not requested: \(history.restore)")
            return
        }
        #expect(try await app.restoreRequests(in: photos.url).map(\.id) == [requestID])
        try fake.finishRestore(requestId: requestID, state: .restored)
        await history.refreshRestore()
        #expect(history.restore == .restored(old, .restored))
    }
}
