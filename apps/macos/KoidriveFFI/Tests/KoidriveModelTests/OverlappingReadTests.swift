// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import KoidriveModel
import Testing

@MainActor
private func history(_ app: AppModel, _ versions: [FileVersion]) throws -> VersionHistoryModel {
    let folder = try #require(app.state.folders.first)
    try #require(app.session.fakeCore()).setVersions(
        spaceId: folder.spaceId, versions: FileVersions(path: "Review.txt", nodeId: "node-review", versions: versions)
    )
    return VersionHistoryModel(app: app, file: folder.url.appending(path: "Review.txt"))
}

private func saved(_ id: String, at ms: Int64, current: Bool = false) -> FileVersion {
    FileVersion(versionId: id, size: 1, savedAtMs: ms, deviceId: "device-1", deviceName: nil, current: current)
}

@MainActor
@Suite struct OverlappingReadTests {
    @Test func aSentRestoreStaysRequestedWhenTheListCannotBeReadAndItsTrashIsReadAgain() async throws {
        let (app, session) = scripted()
        let files = DeletedFilesModel(app: app)
        await files.load()
        let entry = try #require(files.sections.flatMap(\.entries).first)
        files.selection = entry.id
        session.failNextAfterRestore(.restoreRequests)

        await files.restoreSelected()
        #expect(files.restoreError == nil)
        #expect(files.selectedEntry?.isRequested == true)
        #expect(files.hasPendingRestores)
        await files.restoreSelected()
        let requests = try session.base.restoreRequests(path: entry.folder.url.path(percentEncoded: false))
        #expect(requests.count == 1)

        try #require(session.fakeCore()).finishRestore(requestId: requests[0].id, state: .restored)
        session.failNext(.trash)
        await files.refreshRequests()
        #expect(files.hasPendingRestores)
        #expect(files.sections.flatMap(\.entries).contains { $0.id == entry.id })
        await files.refreshRequests()
        #expect(!files.hasPendingRestores)
        #expect(!files.sections.flatMap(\.entries).contains { $0.id == entry.id })
    }

    @Test func aRefreshThatStartedBeforeALoadDoesNotReplaceIt() async throws {
        let (app, session) = scripted()
        let files = DeletedFilesModel(app: app)
        await files.load()
        let folder = try #require(app.state.folders.first)
        #expect(!files.sections.isEmpty)

        session.holdNext(.restoreRequests)
        let refresh = Task { await files.refreshRequests() }
        defer { session.release.signal() }
        try await untilHeld(session)
        for folder in app.state.folders {
            try #require(session.fakeCore()).setTrash(spaceId: folder.spaceId, items: [])
        }
        let load = Task { await files.load() }
        try await Task.sleep(for: .milliseconds(100))
        session.release.signal()
        await refresh.value
        await load.value
        #expect(files.sections.isEmpty)
        #expect(files.scopedFolders.contains { $0.folder == folder })
    }

    @Test func aLateAnswerForAnEarlierRestoreKeepsTheNewerRequest() async throws {
        let (app, session) = scripted()
        let history = try history(app, [saved("v2", at: 2000, current: true), saved("v1", at: 1000)])
        await history.load()
        history.selection = "v1"
        await history.restoreSelected()
        guard case .requested(_, let first) = history.restore else { Issue.record("not requested"); return }
        try #require(session.fakeCore()).finishRestore(requestId: first, state: .refused(reason: .gone))

        session.holdNext(.restoreRequests)
        let late = Task { await history.refreshRestore() }
        defer { session.release.signal() }
        try await untilHeld(session)
        await history.refreshRestore()
        await history.restoreSelected()
        guard case .requested(_, let second) = history.restore else { Issue.record("not requested again"); return }
        session.release.signal()
        await late.value
        #expect(history.restore == .requested(try #require(history.selectedVersion), requestID: second))
        #expect(first != second)
    }

    @Test func anOlderLoadDoesNotReplaceANewerOne() async throws {
        let (app, session) = scripted()
        let history = try history(app, [saved("v1", at: 1000, current: true)])
        session.holdNext(.versions)
        let late = Task { await history.load() }
        defer { session.release.signal() }
        try await untilHeld(session)
        let folder = try #require(app.state.folders.first)
        try #require(session.fakeCore()).setVersions(spaceId: folder.spaceId, versions: FileVersions(
            path: "Review.txt", nodeId: "node-review", versions: [saved("v2", at: 2000, current: true), saved("v1", at: 1000)]
        ))
        await history.load()
        session.release.signal()
        await late.value
        #expect(history.versions.map(\.versionId) == ["v2", "v1"])
    }

    @Test func unlinkingAFolderDropsItsScopeAndItsRestoreTracking() async throws {
        let (app, session) = scripted()
        let files = DeletedFilesModel(app: app)
        await files.load()
        let entry = try #require(files.sections.flatMap(\.entries).first)
        files.scope = .folder(spaceID: entry.id.spaceID)
        files.selection = entry.id
        session.failNextAfterRestore(.restoreRequests)
        await files.restoreSelected()
        #expect(files.hasPendingRestores)

        try await app.unlinkFolder(spaceID: entry.id.spaceID)
        await files.load()
        #expect(files.scope == .all)
        #expect(!files.scopedFolders.isEmpty)
        #expect(!files.hasPendingRestores)
    }
}
