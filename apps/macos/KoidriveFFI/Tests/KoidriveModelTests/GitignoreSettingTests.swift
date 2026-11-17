// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import KoidriveModel
import Testing

@MainActor
struct GitignoreSettingTests {
    private let documentsAndPhotos = GitignoreCount(files: 18_342 + 36_684, bytes: 412_000_000 + 824_000_000,
                                                    folderNames: ["Documents", "Photos"], countedAtMs: 0)

    @Test func turningOffAsksFirstWithTheCountAndCancelKeepsFollowing() async throws {
        let (app, _) = scripted()
        let gitignore = GitignoreSettingModel(app: app)
        #expect(app.followsGitignore)

        await gitignore.set(follow: false)
        #expect(gitignore.confirmingOff)
        #expect(gitignore.follows)
        let count = try #require(gitignore.count)
        #expect(count.files == documentsAndPhotos.files)
        #expect(count.bytes == documentsAndPhotos.bytes)
        #expect(count.folderNames == documentsAndPhotos.folderNames)

        gitignore.cancelOff()
        #expect(!gitignore.confirmingOff)
        #expect(app.followsGitignore)
        #expect(app.session.gitignore())
    }

    @Test func confirmingTurnsItOffAndShowsTheSwitchOffWhileItApplies() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app)
        await gitignore.set(follow: false)

        session.holdNext(.setGitignore)
        let confirming = Task { await gitignore.confirmOff() }
        defer { session.release.signal() }
        try await untilHeld(session)
        #expect(gitignore.applying == false)
        #expect(!gitignore.follows)
        #expect(!gitignore.confirmingOff)

        session.release.signal()
        await confirming.value
        #expect(gitignore.applying == nil)
        #expect(!app.followsGitignore)
        #expect(!gitignore.follows)
        #expect(gitignore.noLongerSyncing == nil)
        #expect(gitignore.failure == nil)
    }

    @Test func turningOnAppliesWithoutAskingThenSaysHowManyFilesStopSyncing() async throws {
        let (app, session) = scripted()
        try session.setGitignore(follow: false)
        let gitignore = GitignoreSettingModel(app: app)
        await gitignore.recount()
        #expect(gitignore.noLongerSyncing == nil)

        session.holdNext(.setGitignore)
        let turningOn = Task { await gitignore.set(follow: true) }
        defer { session.release.signal() }
        try await untilHeld(session)
        #expect(gitignore.applying == true)
        #expect(gitignore.follows)
        #expect(!gitignore.confirmingOff)

        session.release.signal()
        await turningOn.value
        #expect(app.followsGitignore)
        #expect(gitignore.noLongerSyncing == documentsAndPhotos.files)
    }

    @Test func aCountThatFailsLeavesTheFallbackAndStillAppliesTheSetting() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app)

        session.failNext(.gitignoreOnly)
        await gitignore.set(follow: false)
        #expect(gitignore.confirmingOff)
        #expect(gitignore.count == nil)
        await gitignore.confirmOff()
        #expect(!app.followsGitignore)

        session.failNext(.gitignoreOnly)
        await gitignore.set(follow: true)
        #expect(app.followsGitignore)
        #expect(gitignore.noLongerSyncing == nil)
    }

    @Test func aChangeThatIsNotSavedKeepsTheSettingAndSaysWhy() async throws {
        let (app, _) = scripted()
        let gitignore = GitignoreSettingModel(app: app)
        try #require(app.session.fakeCore()).failSaves(fail: true)

        await gitignore.set(follow: false)
        await gitignore.confirmOff()
        #expect(app.followsGitignore)
        #expect(gitignore.follows)
        #expect(gitignore.failure == .NotSaved(reason: "saving is scripted to fail"))

        try #require(app.session.fakeCore()).failSaves(fail: false)
        await gitignore.set(follow: false)
        #expect(gitignore.failure == nil)
    }

    @Test func anOlderCountThatArrivesLastIsDropped() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app)

        session.holdNext(.gitignoreOnly)
        let older = Task { await gitignore.recount() }
        defer { session.release.signal() }
        try await untilHeld(session)
        try await Task.sleep(for: .milliseconds(5))
        await gitignore.recount()
        let newer = try #require(gitignore.count)

        session.release.signal()
        await older.value
        #expect(gitignore.count == newer)
    }

    @Test func theSwitchIgnoresInputWhileAChangeApplies() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app)
        await gitignore.set(follow: false)

        session.holdNext(.setGitignore)
        let confirming = Task { await gitignore.confirmOff() }
        defer { session.release.signal() }
        try await untilHeld(session)
        await gitignore.set(follow: true)
        #expect(gitignore.applying == false)

        session.release.signal()
        await confirming.value
        #expect(!app.followsGitignore)
    }
}
