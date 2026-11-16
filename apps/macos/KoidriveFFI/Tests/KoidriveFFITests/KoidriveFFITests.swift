// SPDX-License-Identifier: Apache-2.0

import Foundation
import os
import Testing

import KoidriveFFI

@Test func versionComesFromTheRustCore() throws {
    let cargoToml = URL(filePath: #filePath)
        .deletingLastPathComponent()
        .appending(path: "../../../../../Cargo.toml")
        .standardizedFileURL
    let manifest = try String(contentsOf: cargoToml, encoding: .utf8)
    let workspacePackage = try #require(
        manifest.components(separatedBy: "[workspace.package]").dropFirst().first?
            .components(separatedBy: "\n[").first
    )
    let workspaceVersion = try #require(
        workspacePackage.firstMatch(of: /^version\s*=\s*"([^"]+)"/.anchorsMatchLineEndings())
    )

    #expect(String(workspaceVersion.1) == version())
}

private final class Recorder: SessionListener {
    let seen = OSAllocatedUnfairLock<[SessionEvent]>(initialState: [])

    func onEvent(event: SessionEvent) {
        seen.withLock { $0.append(event) }
    }

    var statuses: [SyncStatus] {
        seen.withLock { events in
            events.compactMap {
                if case .status(let status) = $0 { status } else { nil }
            }
        }
    }
}

@Test func sessionStreamsEachStatusChange() throws {
    let recorder = Recorder()
    let session = SyncSession.fake(listener: recorder)
    #expect(session.state().status == .stopped)

    session.start()
    #expect(session.state().status == .needsSetup)
    try #require(session.fakeCore()).loadScenario(scenario: .upToDate)
    try session.pause()
    try session.resume()
    session.stop()

    #expect(recorder.statuses == [.needsSetup, .idle, .paused, .idle, .stopped])
}

@Test func signInAndLinkAFolder() throws {
    let session = SyncSession.fake(listener: Recorder())
    let fake = try #require(session.fakeCore())
    fake.failNext(error: .ServerUnreachable(reason: "timed out"))
    #expect(throws: SessionError.ServerUnreachable(reason: "timed out")) {
        try session.connect(url: "https://box.example.com")
    }
    let server = try session.connect(url: "https://box.example.com/")
    #expect(server.url == "https://box.example.com")
    #expect(!server.claimed)
    let account = try session.claim(code: "abcd-efgh-jklm", userName: "Ada", deviceName: "Mac")
    #expect(session.state().account == account)

    let space = try session.createSpace(name: "Documents")
    let folder = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
    try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: false)
    defer { try? FileManager.default.removeItem(at: folder) }
    let linked = try session.linkFolder(spaceId: space.spaceId, path: folder.path(percentEncoded: false))
    #expect(session.state().folders == [linked])
    #expect(throws: SessionError.SpaceAlreadyLinked) {
        try session.linkFolder(spaceId: space.spaceId, path: folder.path(percentEncoded: false))
    }
}

@Test func questionsTakeOnlyTheirOwnAnswers() throws {
    let session = SyncSession.fake(listener: Recorder())
    let fake = try #require(session.fakeCore())
    try fake.loadScenario(scenario: .recovery)
    let questions = session.state().questions
    #expect(questions.count == 5)
    let created = try #require(questions.first {
        if case .recovery(_, .createdAfterRestorePoint) = $0.kind { true } else { false }
    })
    #expect(created.answers == [.keep, .discard])

    #expect(throws: SessionError.AnswerNotAllowed) {
        try session.answer(questionId: created.id, answer: .publish)
    }
    try session.answer(questionId: created.id, answer: .keep)
    #expect(throws: SessionError.QuestionNotFound) {
        try session.answer(questionId: created.id, answer: .keep)
    }
    #expect(fake.givenAnswers() == [GivenAnswer(questionId: created.id, answer: .keep)])
}

@Test func noticesDevicesAndSaveFailuresAreTyped() throws {
    let session = SyncSession.fake(listener: Recorder())
    let fake = try #require(session.fakeCore())
    try fake.loadScenario(scenario: .remoteMassDelete)
    #expect(try session.devices().map(\.thisDevice) == [true, false])
    let notice = try #require(session.state().notices.first)
    guard case .remoteMassDelete(_, let affectedFiles, _, _) = notice.kind else {
        Issue.record("expected a remote mass delete, got \(notice.kind)")
        return
    }
    #expect(affectedFiles == 1_200)

    fake.failSaves(fail: true)
    #expect(throws: SessionError.NotSaved(reason: "saving is scripted to fail")) {
        try session.dismissNotice(noticeId: notice.id)
    }
    #expect(session.state().notices == [notice])
    #expect(session.state().problems.contains {
        if case .stateNotWritable = $0.kind { true } else { false }
    })
    fake.failSaves(fail: false)
    try session.dismissNotice(noticeId: notice.id)
    #expect(session.state().notices.isEmpty)
    #expect(session.state().problems.isEmpty)
}

@Test func aSessionWithoutItsKoiDoesNotOpen() {
    let missing = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString).appending(path: "koi")
    #expect {
        _ = try SyncSession.openWithEventStream(helper: missing)
    } throws: { error in
        guard case .Failed(let reason) = error as? SessionError else { return false }
        return reason.contains("koi is missing")
    }
}

@Test(.timeLimit(.minutes(1)))
func eventStreamFinishesWhenTheSessionIsReleased() async {
    var session: SyncSession?
    let events: AsyncStream<SessionEvent>
    (session, events) = SyncSession.fakeWithEventStream()
    async let seen: [SessionEvent] = {
        var seen: [SessionEvent] = []
        for await event in events {
            seen.append(event)
        }
        return seen
    }()

    session?.start()
    session?.stop()
    session = nil

    #expect(await seen == [
        .questions(questions: []), .notices(notices: []), .status(status: .needsSetup), .status(status: .stopped),
    ])
}

@Test func aCodeKoiCallsWrongIsInvalidCode() throws {
    let directory = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: directory) }
    let koi = directory.appending(path: "koi")
    try """
    #!/bin/sh
    if [ "$2" = join ]; then
      echo '{"outcome":"error","data":null,"error":{"code":"wrong_code","message":"the code is wrong or has expired"}}'
    else
      echo '{"outcome":"ok","data":{"account":null,"running":false,"folders":[]}}'
    fi
    """.write(to: koi, atomically: true, encoding: .utf8)
    try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: koi.path(percentEncoded: false))
    let server = try OneAnswerServer(
        answer: Array("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\n".utf8) + [0x10, 0x01, 0x18, 0x01, 0x28, 0x01]
    )

    let session = try SyncSession.openWithEventStream(helper: koi).0
    _ = try session.connect(url: server.url)
    #expect(throws: SessionError.InvalidCode) {
        try session.pair(code: "AAAA-BBBB", deviceName: "Mac")
    }
}

private final class OneAnswerServer: Sendable {
    let url: String

    init(answer: [UInt8]) throws {
        let listener = socket(AF_INET, SOCK_STREAM, 0)
        var address = sockaddr_in()
        address.sin_family = sa_family_t(AF_INET)
        address.sin_addr.s_addr = inet_addr("127.0.0.1")
        var length = socklen_t(MemoryLayout<sockaddr_in>.size)
        let bound = withUnsafeMutablePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { pointer in
                bind(listener, pointer, length) == 0 && listen(listener, 1) == 0
                    && getsockname(listener, pointer, &length) == 0
            }
        }
        try #require(listener >= 0 && bound)
        url = "http://127.0.0.1:\(UInt16(bigEndian: address.sin_port))"
        Thread.detachNewThread {
            let connection = accept(listener, nil, nil)
            var request = [UInt8](repeating: 0, count: 8192)
            _ = read(connection, &request, request.count)
            _ = answer.withUnsafeBytes { write(connection, $0.baseAddress, $0.count) }
            close(connection)
            close(listener)
        }
    }
}
