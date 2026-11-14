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
        #expect(model.updateNotice == .checking)
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
        let install = Task { await model.installUpdate() }
        await Task.yield()
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
