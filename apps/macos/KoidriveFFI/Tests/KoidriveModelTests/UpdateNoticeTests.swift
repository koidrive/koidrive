// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import KoidriveModel
import Testing

@MainActor
@Suite struct UpdateNoticeTests {
    @Test func eachCheckResultHasItsLine() throws {
        let model = AppModel.fake(scenario: .upToDate)
        guard case .upToDate = model.updateNotice else {
            Issue.record("expected up to date, got \(model.updateNotice)")
            return
        }
        #expect(model.popoverUpdateNotice == nil)

        model.apply(.update(update: .unknown))
        #expect(model.updateNotice == .unchecked)
        model.apply(.update(update: .off))
        #expect(model.updateNotice == .off)
        #expect(model.popoverUpdateNotice == nil)

        let offline = AppModel.fake(scenario: .upToDate)
        offline.apply(.status(status: .offline))
        #expect(offline.updateNotice == .offline)
        #expect(offline.popoverUpdateNotice == nil)
    }

    @Test func anAvailableUpdateShowsInBothPlacesAndStaysInstallableWhenTheSyncServerIsOffline() async throws {
        let model = AppModel.fake(scenario: .updateAvailable)
        let available = UpdateNotice.available(
            version: "0.1.1", releaseURL: URL(string: "https://github.com/koidrive/koidrive/releases/tag/v0.1.1"))
        #expect(model.updateNotice == available)
        #expect(model.popoverUpdateNotice == available)

        model.apply(.status(status: .offline))
        #expect(model.updateNotice == available)
        #expect(model.popoverUpdateNotice == available)
        #expect(await model.installUpdate() == UpdateResult(from: "0.1.0", to: "0.1.1"))
    }

    @Test func installingAnUpdateShowsUpdatingAndThenTheNewState() async throws {
        let model = AppModel.fake(scenario: .updateAvailable)
        let install = Task.immediate { await model.installUpdate() }
        #expect(model.updateNotice == .updating(version: "0.1.1"))
        #expect(model.popoverUpdateNotice == .updating(version: "0.1.1"))
        model.apply(.update(update: .upToDate(checkedAtMs: 1)))
        #expect(model.updateNotice == .updating(version: "0.1.1"))
        #expect(model.popoverUpdateNotice == .updating(version: "0.1.1"))
        #expect(await model.installUpdate() == nil)
        #expect(await install.value == UpdateResult(from: "0.1.0", to: "0.1.1"))
        #expect(!model.updating)
        guard case .upToDate = model.updateNotice else {
            Issue.record("expected up to date, got \(model.updateNotice)")
            return
        }
        #expect(await model.installUpdate() == nil)
    }

    @Test func aFailedUpdateKeepsItsLogAndTerminalCommandUntilTriedAgain() async throws {
        let model = AppModel.fake(scenario: .updateFails)
        #expect(await model.installUpdate() == nil)
        guard case .failed(let failure) = model.updateNotice else {
            Issue.record("expected a failure, got \(model.updateNotice)")
            return
        }
        #expect(failure.version == "0.1.1")
        #expect(failure.logPath?.hasSuffix("/Library/Logs/Koidrive/update.log") == true)
        #expect(failure.fallbackCommand == "brew upgrade --cask koidrive")
        #expect(model.popoverUpdateNotice == .failed(failure))

        let retried = AppModel.fake(scenario: .updateAvailable)
        let unsigned = SessionError.UpdateFailed(reason: "The download is not signed", logPath: "/tmp/update.log", fallbackCommand: nil)
        try #require(retried.session.fakeCore()).failNext(error: unsigned)
        #expect(await retried.installUpdate() == nil)
        guard case .failed(let unsignedFailure) = retried.updateNotice else {
            Issue.record("expected a failure, got \(retried.updateNotice)")
            return
        }
        #expect(unsignedFailure.reason == "The download is not signed")
        #expect(unsignedFailure.logPath == "/tmp/update.log")
        #expect(unsignedFailure.fallbackCommand == nil)
        #expect(await retried.installUpdate() == UpdateResult(from: "0.1.0", to: "0.1.1"))
        #expect(retried.updateFailure == nil)
    }

    @Test func aCheckShowsCheckingThenItsAnswerAndASecondPressJoinsIt() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let fake = try #require(model.session.fakeCore())
        let first = Task.immediate { await model.checkForUpdates() }
        #expect(model.updateNotice == .checking)
        #expect(model.popoverUpdateNotice == .checking)
        #expect(model.checkingForUpdates)
        let second = Task.immediate { await model.checkForUpdates() }
        await first.value
        await second.value
        #expect(fake.updateChecks() == 1)
        #expect(!model.checkingForUpdates)
        guard case .upToDate = model.updateNotice else {
            Issue.record("expected up to date, got \(model.updateNotice)")
            return
        }
        #expect(model.popoverUpdateNotice == model.updateNotice)
        #expect(model.updateNotice.offersCheck)
        model.dismissUpdateCheck()
        #expect(model.popoverUpdateNotice == nil)

        await model.checkForUpdates()
        #expect(fake.updateChecks() == 2)
    }

    @Test func aCheckThatFindsAReleaseShowsTheAvailableNoticeWithUpdate() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        try #require(model.session.fakeCore()).setReleased(version: "0.1.1")
        #expect(model.popoverUpdateNotice == nil)
        await model.checkForUpdates()
        let available = UpdateNotice.available(
            version: "0.1.1", releaseURL: URL(string: "https://github.com/koidrive/koidrive/releases/tag/v0.1.1"))
        #expect(model.updateNotice == available)
        #expect(model.popoverUpdateNotice == available)
        #expect(!available.offersCheck)
        #expect(await model.installUpdate() == UpdateResult(from: "0.1.0", to: "0.1.1"))
    }

    @Test func aFailedCheckSaysWhyUntilANewerAnswerArrives() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let fake = try #require(model.session.fakeCore())
        fake.failNext(error: .ReleasesUnreachable)
        await model.checkForUpdates()
        #expect(model.updateNotice == .checkFailed(.unreachable))
        #expect(model.popoverUpdateNotice == .checkFailed(.unreachable))
        #expect(model.updateNotice.offersCheck)

        fake.failNext(error: .ReleasesRefused)
        await model.checkForUpdates()
        #expect(model.updateNotice == .checkFailed(.refused))
        fake.failNext(error: .Failed(reason: "koi is missing"))
        await model.checkForUpdates()
        #expect(model.updateNotice == .checkFailed(.other(.Failed(reason: "koi is missing"))))

        let later = Int64(Date.now.timeIntervalSince1970 * 1000) + 60_000
        model.apply(.update(update: .upToDate(checkedAtMs: later)))
        #expect(model.updateNotice == .upToDate(checkedAtMs: later))
    }

    @Test func aCheckAfterAFailedInstallShowsItsAnswerAndUpdateBringsTheFailureBack() async throws {
        let model = AppModel.fake(scenario: .updateFails)
        #expect(await model.installUpdate() == nil)
        guard case .failed(let failure) = model.updateNotice else {
            Issue.record("expected the install failure, got \(model.updateNotice)")
            return
        }
        let check = Task.immediate { await model.checkForUpdates() }
        #expect(model.updateNotice == .checking)
        #expect(model.popoverUpdateNotice == .checking)
        await check.value
        guard case .available(let version, _) = model.updateNotice else {
            Issue.record("expected the check's answer, got \(model.updateNotice)")
            return
        }
        #expect(version == failure.version)
        #expect(model.popoverUpdateNotice == model.updateNotice)

        #expect(await model.installUpdate() == nil)
        guard case .failed(let again) = model.updateNotice else {
            Issue.record("expected the install failure again, got \(model.updateNotice)")
            return
        }
        #expect(again.logPath == failure.logPath)
        #expect(again.fallbackCommand == failure.fallbackCommand)

        try #require(model.session.fakeCore()).failNext(error: .ReleasesUnreachable)
        await model.checkForUpdates()
        #expect(model.updateNotice == .checkFailed(.unreachable))
        #expect(model.popoverUpdateNotice == .checkFailed(.unreachable))
    }

    @Test func aCheckAnswersWhileSyncIsOfflineAndWhileScheduledChecksAreOff() async throws {
        let offline = AppModel.fake(scenario: .upToDate)
        offline.apply(.status(status: .offline))
        #expect(offline.updateNotice == .offline)
        #expect(offline.updateNotice.offersCheck)
        await offline.checkForUpdates()
        guard case .upToDate = offline.updateNotice else {
            Issue.record("expected up to date, got \(offline.updateNotice)")
            return
        }

        let off = AppModel.fake(scenario: .upToDate)
        let fake = try #require(off.session.fakeCore())
        fake.setUpdate(update: .off)
        off.apply(.update(update: .off))
        #expect(off.updateNotice == .off)
        #expect(off.updateNotice.offersCheck)
        fake.setReleased(version: "0.1.1")
        await off.checkForUpdates()
        #expect(off.state.update == .off)
        guard case .available(let version, _) = off.updateNotice else {
            Issue.record("expected the one-off answer, got \(off.updateNotice)")
            return
        }
        #expect(version == "0.1.1")
    }

    @Test func theUpdatedNoteShowsInThePopoverUntilAcknowledged() throws {
        let suite = "dev.koidrive.tests.\(UUID().uuidString)"
        let defaults = try #require(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        func launch(_ version: String) -> AppModel {
            let (session, events) = SyncSession.fakeWithEventStream()
            return AppModel(session: session, events: events, defaults: defaults, version: version)
        }
        _ = launch("0.1.0")
        let updated = launch("0.1.1")
        #expect(updated.popoverUpdateNotice == .updated(version: "0.1.1"))
        if case .updated = updated.updateNotice { Issue.record("Settings shows the check, not the note") }
        updated.acknowledgeUpdate()
        #expect(updated.popoverUpdateNotice == nil)
    }
}
