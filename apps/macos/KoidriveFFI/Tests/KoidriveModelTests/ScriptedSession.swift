// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import KoidriveModel
import Testing

final class ScriptedSession: SyncSession, @unchecked Sendable {
    enum Read: Hashable {
        case restoreRequests
        case trash
        case versions
        case shareLinks
        case createShareLink
        case answer
    }

    let base: SyncSession
    let release = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private var failing: Set<Read> = []
    private var failingAfterRestore: Set<Read> = []
    private var holding: Read?
    private var held = false

    init(base: SyncSession) {
        self.base = base
        super.init(noHandle: .init())
    }

    required init(unsafeFromHandle handle: UInt64) { fatalError("unused") }

    func failNext(_ read: Read) { lock.withLock { _ = failing.insert(read) } }
    func failNextAfterRestore(_ read: Read) { lock.withLock { _ = failingAfterRestore.insert(read) } }
    func holdNext(_ read: Read) { lock.withLock { holding = read; held = false } }
    var isHolding: Bool { lock.withLock { held } }

    override func state() -> SessionState { base.state() }
    override func telemetry() -> TelemetrySetting { base.telemetry() }
    override func fakeCore() -> FakeCore? { base.fakeCore() }
    override func unlinkFolder(spaceId: String) throws { try base.unlinkFolder(spaceId: spaceId) }

    override func restoreDeleted(path: String, nodeId: String) throws -> RestoreAsked {
        let asked = try base.restoreDeleted(path: path, nodeId: nodeId)
        lock.withLock {
            failing.formUnion(failingAfterRestore)
            failingAfterRestore = []
        }
        return asked
    }

    override func restoreVersion(path: String, versionId: String) throws -> RestoreAsked {
        try base.restoreVersion(path: path, versionId: versionId)
    }

    override func restoreRequests(path: String) throws -> [RestoreRequest] {
        try run(.restoreRequests) { try base.restoreRequests(path: path) }
    }

    override func trash(path: String) throws -> Trash {
        try run(.trash) { try base.trash(path: path) }
    }

    override func versions(path: String) throws -> FileVersions {
        try run(.versions) { try base.versions(path: path) }
    }

    override func shareLinks() throws -> [ShareLink] {
        try run(.shareLinks) { try base.shareLinks() }
    }

    override func createShareLink(path: String, expiry: ShareExpiry, password: String?) throws -> NewShareLink {
        try run(.createShareLink) { try base.createShareLink(path: path, expiry: expiry, password: password) }
    }

    override func revokeShareLink(linkId: String) throws -> ShareLink {
        try base.revokeShareLink(linkId: linkId)
    }

    override func answer(questionId: String, answer: Answer) throws {
        try run(.answer) { try base.answer(questionId: questionId, answer: answer) }
    }

    private func run<T>(_ read: Read, _ call: () throws -> T) throws -> T {
        if lock.withLock({ failing.remove(read) != nil }) {
            throw SessionError.ServerUnreachable(reason: "scripted")
        }
        let answer = try call()
        let holds = lock.withLock {
            guard holding == read else { return false }
            holding = nil
            held = true
            return true
        }
        if holds { _ = release.wait(timeout: .now() + 5) }
        return answer
    }
}

@MainActor
func scripted(_ scenario: FakeScenario = .upToDate) -> (AppModel, ScriptedSession) {
    let session = ScriptedSession(base: AppModel.fake(scenario: scenario).session)
    return (AppModel(session: session, events: AsyncStream { $0.finish() }), session)
}

@MainActor
func untilHeld(_ session: ScriptedSession) async throws {
    for _ in 0..<1000 where !session.isHolding {
        try await Task.sleep(for: .milliseconds(2))
    }
    try #require(session.isHolding)
}
