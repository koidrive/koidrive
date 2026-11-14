// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import KoidriveModel
import Testing

private final class ScriptedSession: SyncSession, @unchecked Sendable {
    let base: SyncSession
    let release = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private var failRequestsAfterRestore = false
    private var failRequests = false
    private var failTrash = false
    private var holdRequests = false
    private var holdVersions = false
    private var held = false

    init(base: SyncSession) {
        self.base = base
        super.init(noHandle: .init())
    }

    required init(unsafeFromHandle handle: UInt64) { fatalError("unused") }

    func failFirstRequestsReadAfterRestore() { lock.withLock { failRequestsAfterRestore = true } }
    func failNextTrashRead() { lock.withLock { failTrash = true } }
    func holdNextRequestsRead() { lock.withLock { holdRequests = true; held = false } }
    func holdNextVersionsRead() { lock.withLock { holdVersions = true; held = false } }
    var isHolding: Bool { lock.withLock { held } }

    override func state() -> SessionState { base.state() }
    override func telemetry() -> TelemetrySetting { base.telemetry() }
    override func fakeCore() -> FakeCore? { base.fakeCore() }

    override func restoreDeleted(path: String, nodeId: String) throws -> RestoreAsked {
        let asked = try base.restoreDeleted(path: path, nodeId: nodeId)
        lock.withLock {
            failRequests = failRequestsAfterRestore
            failRequestsAfterRestore = false
        }
        return asked
    }

    override func restoreVersion(path: String, versionId: String) throws -> RestoreAsked {
        try base.restoreVersion(path: path, versionId: versionId)
    }

    override func restoreRequests(path: String) throws -> [RestoreRequest] {
        if lock.withLock({ defer { failRequests = false }; return failRequests }) {
            throw SessionError.ServerUnreachable(reason: "scripted")
        }
        let requests = try base.restoreRequests(path: path)
        hold(\.holdRequests)
        return requests
    }

    override func trash(path: String) throws -> Trash {
        if lock.withLock({ defer { failTrash = false }; return failTrash }) {
            throw SessionError.ServerUnreachable(reason: "scripted")
        }
        return try base.trash(path: path)
    }

    override func versions(path: String) throws -> FileVersions {
        let versions = try base.versions(path: path)
        hold(\.holdVersions)
        return versions
    }

    private func hold(_ flag: ReferenceWritableKeyPath<ScriptedSession, Bool>) {
        let holds = lock.withLock {
            defer { self[keyPath: flag] = false }
            if self[keyPath: flag] { held = true }
            return self[keyPath: flag]
        }
        if holds { _ = release.wait(timeout: .now() + 5) }
    }
}

@MainActor
private func scripted() -> (AppModel, ScriptedSession) {
    let session = ScriptedSession(base: AppModel.fake(scenario: .upToDate).session)
    return (AppModel(session: session, events: AsyncStream { $0.finish() }), session)
}

@MainActor
private func untilHeld(_ session: ScriptedSession) async throws {
    for _ in 0..<1000 where !session.isHolding {
        try await Task.sleep(for: .milliseconds(2))
    }
    try #require(session.isHolding)
}

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
        session.failFirstRequestsReadAfterRestore()

        await files.restoreSelected()
        #expect(files.restoreError == nil)
        #expect(files.selectedEntry?.isRequested == true)
        #expect(files.hasPendingRestores)
        await files.restoreSelected()
        let requests = try session.base.restoreRequests(path: entry.folder.url.path(percentEncoded: false))
        #expect(requests.count == 1)

        try #require(session.fakeCore()).finishRestore(requestId: requests[0].id, state: .restored)
        session.failNextTrashRead()
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

        session.holdNextRequestsRead()
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

        session.holdNextRequestsRead()
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
        session.holdNextVersionsRead()
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
}
