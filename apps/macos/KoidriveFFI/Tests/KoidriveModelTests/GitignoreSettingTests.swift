// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import KoidriveModel
import Testing

@MainActor
struct GitignoreSettingTests {
    private let files: UInt64 = 18_342 + 36_684

    @Test func turningOffAsksFirstWithTheCountAndCancelKeepsFollowing() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app, countWait: .seconds(30))
        #expect(app.followsGitignore)

        await gitignore.set(follow: false)
        #expect(gitignore.confirmingOff)
        #expect(gitignore.follows)
        let count = try #require(gitignore.count)
        #expect(count.files == files)
        #expect(count.bytes == 412_000_000 + 824_000_000)
        #expect(count.folderNames == ["Documents", "Photos"])

        gitignore.cancelOff()
        try await Task.sleep(for: .milliseconds(100))
        await gitignore.recount()
        #expect(!gitignore.confirmingOff)
        #expect(gitignore.applying == nil)
        #expect(app.followsGitignore)
        #expect(session.gitignore())
        #expect(session.calls(.setGitignore) == 0)
    }

    @Test func theAlertClosingAfterSyncFilesStillTurnsItOff() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app)
        await gitignore.set(follow: false)

        let confirming = gitignore.confirmOff()
        gitignore.cancelOff()
        await confirming?.value
        #expect(!app.followsGitignore)
        #expect(session.calls(.setGitignore) == 1)
    }

    @Test func aSlowCountAsksWithTheFallbackOnceTheWaitIsOver() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app, countWait: .milliseconds(50))

        session.holdNext(.gitignoreOnly)
        let asking = Task { await gitignore.set(follow: false) }
        defer { session.release.signal() }
        for _ in 0..<2500 where !gitignore.confirmingOff {
            try await Task.sleep(for: .milliseconds(2))
        }
        #expect(session.isHolding)
        #expect(gitignore.confirmingOff)
        #expect(gitignore.count == nil)

        session.release.signal()
        await asking.value
        await gitignore.recount()
        #expect(gitignore.confirmingOff)
        #expect(gitignore.count?.files == files)
    }

    @Test func confirmingTurnsItOffAndShowsTheSwitchOffWhileItApplies() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app)
        await gitignore.set(follow: false)

        session.holdNext(.setGitignore)
        let confirming = gitignore.confirmOff()
        defer { session.release.signal() }
        try await untilHeld(session)
        #expect(gitignore.applying == false)
        #expect(!gitignore.follows)
        #expect(!gitignore.confirmingOff)

        session.release.signal()
        await confirming?.value
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
        #expect(gitignore.noLongerSyncing == files)
    }

    @Test func theTurnedOnLineCountsOnlyAWalkThatStartedAfterTheChange() async throws {
        let (app, session) = scripted()
        try session.setGitignore(follow: false)
        let gitignore = GitignoreSettingModel(app: app)
        await gitignore.recount()

        session.holdNext(.gitignoreOnly)
        let earlier = Task { await gitignore.recount() }
        defer { session.release.signal() }
        try await untilHeld(session)
        try await Task.sleep(for: .milliseconds(5))

        await gitignore.set(follow: true)
        #expect(gitignore.noLongerSyncing == files)
        let afterTurningOn = try #require(gitignore.count)
        #expect(session.calls(.gitignoreOnly) == 2 + 1 + 2)

        try await Task.sleep(for: .milliseconds(5))
        session.release.signal()
        await earlier.value
        #expect(gitignore.count == afterTurningOn)
    }

    @Test func turningOffWhileTheTurnedOnCountRunsShowsNoLine() async throws {
        let (app, session) = scripted()
        try session.setGitignore(follow: false)
        let gitignore = GitignoreSettingModel(app: app, countWait: .milliseconds(50))
        await gitignore.recount()

        session.holdNext(.gitignoreOnly)
        let turningOn = Task { await gitignore.set(follow: true) }
        defer { session.release.signal() }
        try await untilHeld(session)
        #expect(app.followsGitignore)

        await gitignore.set(follow: false)
        #expect(gitignore.confirmingOff)
        session.release.signal()
        await turningOn.value
        #expect(gitignore.noLongerSyncing == nil)
    }

    @Test func aTurnedOnCountThatFailsLeavesNoLineForALaterTurnOff() async throws {
        let (app, session) = scripted()
        try session.setGitignore(follow: false)
        let gitignore = GitignoreSettingModel(app: app, countWait: .seconds(30))
        await gitignore.recount()

        session.failNext(.gitignoreOnly)
        await gitignore.set(follow: true)
        #expect(app.followsGitignore)
        #expect(gitignore.noLongerSyncing == nil)

        await gitignore.set(follow: false)
        #expect(gitignore.confirmingOff)
        #expect(gitignore.count?.files == files)
        #expect(gitignore.noLongerSyncing == nil)
    }

    @Test func aCountThatFailsLeavesTheFallbackAndStillAppliesTheSetting() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app)

        session.failNext(.gitignoreOnly)
        await gitignore.set(follow: false)
        #expect(gitignore.confirmingOff)
        #expect(gitignore.count == nil)
        await gitignore.confirmOff()?.value
        #expect(!app.followsGitignore)

        session.failNext(.gitignoreOnly)
        await gitignore.set(follow: true)
        #expect(app.followsGitignore)
        #expect(gitignore.noLongerSyncing == nil)
    }

    @Test func aCountThatLeavesOutAnUnreadableFolderShowsNoNumbers() async throws {
        let (app, session) = scripted()
        try session.setGitignore(follow: false)
        let gitignore = GitignoreSettingModel(app: app)
        session.countLeavesOut(["/Users/ada/Documents/pgdata"])
        await gitignore.recount()
        #expect(gitignore.count == nil)

        await gitignore.set(follow: true)
        #expect(session.calls(.setGitignore) == 2)
        #expect(app.followsGitignore)
        #expect(gitignore.noLongerSyncing == nil)

        await gitignore.set(follow: false)
        #expect(gitignore.confirmingOff)
        #expect(gitignore.count == nil)
    }

    @Test func aChangeThatIsNotSavedKeepsTheSettingAndSaysWhy() async throws {
        let (app, _) = scripted()
        let gitignore = GitignoreSettingModel(app: app)
        try #require(app.session.fakeCore()).failSaves(fail: true)

        await gitignore.set(follow: false)
        await gitignore.confirmOff()?.value
        #expect(app.followsGitignore)
        #expect(gitignore.follows)
        #expect(gitignore.failure == .NotSaved(reason: "saving is scripted to fail"))

        try #require(app.session.fakeCore()).failSaves(fail: false)
        await gitignore.set(follow: false)
        #expect(gitignore.failure == nil)
    }

    @Test func countsAskedForWhileOneRunsShareIt() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app)

        session.holdNext(.gitignoreOnly)
        let first = Task { await gitignore.recount() }
        defer { session.release.signal() }
        try await untilHeld(session)
        let second = Task { await gitignore.recount() }
        try await Task.sleep(for: .milliseconds(20))

        session.release.signal()
        await first.value
        await second.value
        #expect(gitignore.count?.files == files)
        #expect(session.calls(.gitignoreOnly) == 2)
    }

    @Test func aCancelledCountStopsBeforeTheNextFolder() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app)

        session.holdNext(.gitignoreOnly)
        let counting = Task { await gitignore.recount() }
        defer { session.release.signal() }
        try await untilHeld(session)
        counting.cancel()

        session.release.signal()
        await counting.value
        #expect(gitignore.count == nil)
        #expect(session.calls(.gitignoreOnly) == 1)
    }

    @Test func aCountAskedForAfterACancelledOneStillCounts() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app)

        session.holdNext(.gitignoreOnly)
        let left = Task { await gitignore.recount() }
        defer { session.release.signal() }
        try await untilHeld(session)
        left.cancel()
        let back = Task { await gitignore.recount() }
        try await Task.sleep(for: .milliseconds(20))

        session.release.signal()
        await left.value
        await back.value
        #expect(gitignore.count?.files == files)
    }

    @Test func leavingTheTabWhileTheTurnedOnCountRunsKeepsTheLine() async throws {
        let (app, session) = scripted()
        try session.setGitignore(follow: false)
        let gitignore = GitignoreSettingModel(app: app)
        await gitignore.recount()

        session.holdNext(.gitignoreOnly)
        let turningOn = Task { await gitignore.set(follow: true) }
        defer { session.release.signal() }
        try await untilHeld(session)
        let visit = Task { await gitignore.recount() }
        try await Task.sleep(for: .milliseconds(20))
        visit.cancel()
        try await Task.sleep(for: .milliseconds(20))

        session.release.signal()
        await turningOn.value
        await visit.value
        #expect(gitignore.noLongerSyncing == files)
    }

    @Test func theSwitchIgnoresInputWhileAChangeApplies() async throws {
        let (app, session) = scripted()
        let gitignore = GitignoreSettingModel(app: app)
        await gitignore.set(follow: false)

        session.holdNext(.setGitignore)
        let confirming = gitignore.confirmOff()
        defer { session.release.signal() }
        try await untilHeld(session)
        await gitignore.set(follow: true)
        #expect(gitignore.applying == false)

        session.release.signal()
        await confirming?.value
        #expect(!app.followsGitignore)
    }
}
