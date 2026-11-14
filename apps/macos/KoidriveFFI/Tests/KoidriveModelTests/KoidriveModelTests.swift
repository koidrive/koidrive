// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import Foundation
import Testing

import KoidriveModel

private func temporaryDirectory() throws -> URL {
    let url = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}

private func item(_ path: String) -> HeldItem {
    HeldItem(nodeId: path, path: path, kind: .file, files: 1, detail: .plain)
}

@MainActor
private final class Fetches {
    var pending: [CheckedContinuation<[SpaceStorage], any Error>] = []

    func next() async throws -> [SpaceStorage] {
        try await withCheckedThrowingContinuation { pending.append($0) }
    }

    func waitFor(_ count: Int) async {
        while pending.count < count { await Task.yield() }
    }
}

@MainActor
@Suite struct AppModelTests {
    @Test func eventsReplaceTheirPartOfTheState() {
        let model = AppModel.fake()
        model.apply(.status(status: .syncing))
        model.apply(.progress(progress: SyncProgress(
            filesToUpload: 1, filesToDownload: 2, bytesDone: 25, bytesTotal: 100, current: nil
        )))
        #expect(model.state.status == .syncing)
        #expect(model.progressFraction == 0.25)

        model.apply(.progress(progress: nil))
        #expect(model.progressFraction == nil)
        model.apply(.progress(progress: SyncProgress(
            filesToUpload: 0, filesToDownload: 0, bytesDone: 0, bytesTotal: 0, current: nil
        )))
        #expect(model.progressFraction == nil)
    }

    @Test func firstRunNeedsSetupAndStartsInTheSetupStep() {
        let model = AppModel.fake(scenario: .firstRun)
        model.start()
        #expect(model.needsSetup)
        #expect(model.state.status == .needsSetup)
        #expect(SetupModel(app: model, mode: .firstRun, deviceName: "Mac").step == .server)
        #expect(SetupModel(app: model, mode: .addFolder, deviceName: "Mac").step == .server)

        let ready = AppModel.fake(scenario: .upToDate)
        ready.start()
        #expect(!ready.needsSetup)
        #expect(ready.state.status == .idle)
        #expect(SetupModel(app: ready, mode: .firstRun, deviceName: "Mac").step == .done)
        #expect(SetupModel(app: ready, mode: .addFolder, deviceName: "Mac").step == .folder)
    }

    @Test func onlyADebugBuildStartsOnTheFakeScenarioPreference() throws {
        let suite = "dev.koidrive.tests.\(UUID().uuidString)"
        let defaults = try #require(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        defaults.set("massDelete", forKey: "FakeScenario")
        #if DEBUG
        #expect(AppModel.launchScenario(in: defaults) == .massDelete)
        #else
        #expect(AppModel.launchScenario(in: defaults) == nil)
        #endif

        defaults.set("noSuchScenario", forKey: "FakeScenario")
        #expect(AppModel.launchScenario(in: defaults) == nil)
    }

    @Test func storageListsEachSpaceWithItsTotal() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let storage = try await model.storage()
        let spaces = try await model.spaces()
        #expect(storage.map(\.space) == spaces)
        let usage = try #require(storage.first).usage.get()
        #expect(usage.totalBytes > 0)
        #expect(usage.totalBytes == usage.liveBytes + usage.trashBytes + usage.oldVersionBytes + usage.unreferencedBytes)
    }

    @Test func storageKeepsTheSpacesThatAnswer() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let spaces = try await model.spaces()
        try #require(spaces.count > 1)
        let refusal = SessionError.ServerUnreachable(reason: "space closed for a rewind")
        try #require(model.session.fakeCore()).refuseSpaceUsage(spaceId: spaces[0].spaceId, error: refusal)
        let storage = try await model.storage()
        #expect(storage.map(\.space) == spaces)
        #expect(storage[0].usage == .failure(refusal))
        for answered in storage.dropFirst() {
            #expect(try answered.usage.get().totalBytes > 0)
        }
    }

    @Test func storageListsEveryRefusal() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let spaces = try await model.spaces()
        let fake = try #require(model.session.fakeCore())
        for space in spaces {
            fake.refuseSpaceUsage(spaceId: space.spaceId, error: .SpaceNotFound)
        }
        let storage = try await model.storage()
        #expect(storage.map(\.space) == spaces)
        #expect(storage.map(\.usage) == spaces.map { _ in Result<SpaceUsage, SessionError>.failure(.SpaceNotFound) })
    }

    @Test func storageFailsWhenNotSignedIn() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let space = try #require(try await model.spaces().last)
        try #require(model.session.fakeCore()).refuseSpaceUsage(spaceId: space.spaceId, error: .NotSignedIn)
        await #expect(throws: SessionError.NotSignedIn) { try await model.storage() }
    }

    @Test func storageKeepsTheNewestLoad() async throws {
        let fresh = try await AppModel.fake(scenario: .upToDate).storage()
        let storage = StorageModel()
        let fetches = Fetches()
        let older = Task { await storage.load(fetches.next) }
        await fetches.waitFor(1)
        let newer = Task { await storage.load(fetches.next) }
        await fetches.waitFor(2)
        fetches.pending[1].resume(returning: fresh)
        await newer.value
        fetches.pending[0].resume(throwing: SessionError.ServerUnreachable(reason: "timed out"))
        await older.value
        #expect(storage.spaces == fresh)
        #expect(storage.error == nil)
    }

    @Test func storageDropsACancelledLoad() async throws {
        let fresh = try await AppModel.fake(scenario: .upToDate).storage()
        let storage = StorageModel()
        let fetches = Fetches()
        let load = Task { await storage.load(fetches.next) }
        await fetches.waitFor(1)
        load.cancel()
        fetches.pending[0].resume(returning: fresh)
        await load.value
        #expect(storage.spaces == nil)
        #expect(storage.error == nil)
    }

    @Test func pauseAndResume() throws {
        let model = AppModel.fake(scenario: .upToDate)
        model.start()
        try model.togglePause()
        #expect(model.isPaused)
        try model.togglePause()
        #expect(!model.isPaused)

        try #require(model.session.fakeCore()).failSaves(fail: true)
        #expect(throws: SessionError.NotSaved(reason: "saving is scripted to fail")) {
            try model.togglePause()
        }
        #expect(!model.isPaused)
        #expect(model.state.problems.count == 1)
        model.stop()
        #expect(model.state.status == .stopped)
    }

    @Test func theUsageReportSwitchTurnsReportsOnAndOff() throws {
        let model = AppModel.fake(scenario: .upToDate)
        model.start()
        #expect(model.telemetry == TelemetrySetting(enabled: true, decided: true, locked: false))
        try model.setTelemetry(enabled: false)
        #expect(model.telemetry == TelemetrySetting(enabled: false, decided: true, locked: false))
        try model.setTelemetry(enabled: true)
        #expect(model.telemetry == TelemetrySetting(enabled: true, decided: true, locked: false))

        try #require(model.session.fakeCore()).failSaves(fail: true)
        #expect(throws: SessionError.NotSaved(reason: "saving is scripted to fail")) {
            try model.setTelemetry(enabled: false)
        }
        #expect(model.telemetry.enabled)
    }

    @Test func aLockedUsageReportSettingIsReportedLockedAndKeepsItsState() throws {
        let model = AppModel.fake(scenario: .upToDate)
        try #require(model.session.fakeCore()).setTelemetrySetting(
            setting: TelemetrySetting(enabled: false, decided: false, locked: true))
        model.start()
        #expect(model.telemetry.locked)

        try model.setTelemetry(enabled: true)
        #expect(model.telemetry == TelemetrySetting(enabled: false, decided: true, locked: true))
    }

    @Test func onlyAVariableThatTurnsUsageReportsOffIsExplained() throws {
        let cases: [(TelemetrySetting, Bool)] = [
            (TelemetrySetting(enabled: false, decided: false, locked: false), false),
            (TelemetrySetting(enabled: true, decided: true, locked: false), false),
            (TelemetrySetting(enabled: false, decided: true, locked: false), false),
            (TelemetrySetting(enabled: false, decided: false, locked: true), true),
            (TelemetrySetting(enabled: true, decided: true, locked: true), false),
        ]
        for (setting, explained) in cases {
            let model = AppModel.fake(scenario: .upToDate)
            try #require(model.session.fakeCore()).setTelemetrySetting(setting: setting)
            model.start()
            #expect(model.usageReportsTurnedOffByVariable == explained, "\(setting)")
        }
    }

    @Test func unansweredQuestionsAreUnseenAtLaunch() throws {
        var model = AppModel.fake(scenario: .recovery)
        model.markQuestionsSeen()
        #expect(!model.hasUnseenQuestions)
        let questions = model.state.questions

        model = AppModel(session: model.session, events: AsyncStream { $0.finish() })
        #expect(model.state.questions == questions)
        #expect(model.unseenQuestionIDs == Set(questions.map(\.id)))
        model.markQuestionsSeen()
        #expect(!model.hasUnseenQuestions)
    }

    @Test func onlyNewQuestionsBecomeUnseen() throws {
        let model = AppModel.fake(scenario: .ambiguousDisappearance)
        let fake = try #require(model.session.fakeCore())
        model.markQuestionsSeen()
        let old = try #require(model.state.questions.first)

        let new = try fake.ask(spaceId: old.spaceId, kind: .ambiguousDisappearance, items: [item("a")])
        model.apply(.questions(questions: model.session.state().questions))
        #expect(model.unseenQuestionIDs == [new])

        try fake.withdrawQuestion(questionId: new)
        model.apply(.questions(questions: model.session.state().questions))
        #expect(!model.hasUnseenQuestions)
    }

    @Test func answeringRemovesTheQuestionAndRefusesOtherAnswers() async throws {
        let model = AppModel.fake(scenario: .massDelete)
        let question = try #require(model.state.questions.first)
        #expect(question.answers == [.delete, .restore])
        #expect(question.affectedFiles == 4_553)
        #expect(Answer.delete.isDestructive && !Answer.restore.isDestructive)

        await #expect(throws: SessionError.AnswerNotAllowed) {
            try await model.answer(question, with: .publish)
        }
        try await model.answer(question, with: .restore)
        #expect(model.state.questions.isEmpty)
        #expect(model.attentionCount == 0)
        await #expect(throws: SessionError.QuestionNotFound) {
            try await model.answer(question, with: .restore)
        }
    }

    @Test func questionSectionsGroupBySpaceAndOrderRecoveryGroups() throws {
        let model = AppModel.fake(scenario: .manyQuestions)
        let sections = model.questionSections
        #expect(sections.map(\.folder?.spaceName) == ["Documents", "Photos"])
        #expect(sections.reduce(0) { $0 + $1.questions.count } == 31)

        let documents = sections[0]
        #expect(documents.recoveryCause == nil)
        if case .massDelete = documents.questions[0].kind {} else {
            Issue.record("the mass delete comes first")
        }
        #expect(documents.questions[0].items.count == 1_500)

        let photos = sections[1]
        #expect(photos.recoveryCause?.trigger == .spaceRewound)
        #expect(photos.questions.map(\.recoveryGroup) == [
            .createdAfterRestorePoint, .changedOnThisDevice, .createdOnThisDevice,
            .deletedOnThisDevice, .requestedBeforeReset,
        ])
        #expect(photos.questions.allSatisfy { $0.items.count == 300 })
    }

    @Test func problemSectionsPutSessionProblemsFirst() {
        let model = AppModel.fake(scenario: .problems)
        let sections = model.problemSections
        #expect(sections.map(\.spaceID) == [nil, "space-documents", "space-photos", "space-archive"])
        #expect(sections.reduce(0) { $0 + $1.problems.count } == 19)
        #expect(model.attentionCount == 19)
        #expect(model.folder(spaceID: "space-photos")?.state == .halted)
    }

    @Test func headlineAndIconBadgeFollowThePriority() throws {
        #expect(AppModel.fake(scenario: .firstRun).headline == .setup)
        #expect(AppModel.fake(scenario: .firstRun).iconBadge == .none)
        let expected: [(FakeScenario, Headline, IconBadge)] = [
            (.upToDate, .status(.idle), .none),
            (.syncing, .status(.syncing), .syncing),
            (.paused, .status(.paused), .paused),
            (.offline, .status(.offline), .offline),
            (.massDelete, .questions, .question),
            (.problems, .halted, .problem),
        ]
        for (scenario, headline, badge) in expected {
            let model = AppModel.fake(scenario: scenario)
            model.start()
            #expect(model.headline == headline, "\(scenario)")
            #expect(model.iconBadge == badge, "\(scenario)")
        }

        let model = AppModel.fake(scenario: .upToDate)
        model.start()
        let fake = try #require(model.session.fakeCore())
        let file = try fake.reportProblem(spaceId: "space-documents", path: "log.txt", kind: .fileBusy)
        model.apply(.problems(problems: model.session.state().problems))
        #expect(model.headline == .status(.idle))
        #expect(model.iconBadge == .problem)
        try fake.resolveProblem(problemId: file)
        model.apply(.problems(problems: model.session.state().problems))
        #expect(model.iconBadge == .none)
    }

    @Test func syncingArrowsTurnOnlyWhileSyncing() throws {
        let expected: [(FakeScenario, menuBar: Bool, headline: Bool)] = [
            (.firstRun, false, false),
            (.upToDate, false, false),
            (.syncing, true, true),
            (.paused, false, false),
            (.offline, false, false),
            (.massDelete, false, false),
            (.problems, false, false),
        ]
        for (scenario, menuBar, headline) in expected {
            let model = AppModel.fake(scenario: scenario)
            model.start()
            #expect(model.turnsMenuBarArrows(reduceMotion: false) == menuBar, "\(scenario)")
            #expect(model.turnsHeadlineArrows(reduceMotion: false) == headline, "\(scenario)")
            #expect(!model.turnsMenuBarArrows(reduceMotion: true), "\(scenario)")
            #expect(!model.turnsHeadlineArrows(reduceMotion: true), "\(scenario)")
        }

        let model = AppModel.fake(scenario: .upToDate)
        model.start()
        model.apply(.status(status: .connecting))
        #expect(model.turnsMenuBarArrows(reduceMotion: false))
        #expect(model.turnsHeadlineArrows(reduceMotion: false))

        model.apply(.status(status: .syncing))
        let fake = try #require(model.session.fakeCore())
        _ = try fake.reportProblem(spaceId: "space-documents", path: "log.txt", kind: .fileBusy)
        model.apply(.problems(problems: model.session.state().problems))
        #expect(!model.turnsMenuBarArrows(reduceMotion: false))
        #expect(model.turnsHeadlineArrows(reduceMotion: false))

        model.apply(.status(status: .idle))
        #expect(!model.turnsHeadlineArrows(reduceMotion: false))
    }

    @Test func questionsInTheOnlyLinkedFolderShowNoSpace() async throws {
        let model = AppModel.fake(scenario: .massDelete)
        try await model.unlinkFolder(spaceID: "space-photos")
        #expect(model.state.folders.map(\.spaceName) == ["Documents"])
        #expect(model.questionSections.map(\.folder?.spaceName) == ["Documents"])
        #expect(!model.questionSectionsShowSpace)
    }

    @Test func questionsShowTheirSpaceWhenTwoFoldersAreLinked() throws {
        let model = AppModel.fake(scenario: .massDelete)
        #expect(model.state.folders.count == 2)
        #expect(model.questionSections.map(\.folder?.spaceName) == ["Documents"])
        #expect(model.questionSectionsShowSpace)
    }

    @Test func aQuestionOfAnUnlinkedSpaceShowsTheSpace() async throws {
        let model = AppModel.fake(scenario: .massDelete)
        try await model.unlinkFolder(spaceID: "space-photos")
        _ = try #require(model.session.fakeCore()).ask(spaceId: "space-gone", kind: .ambiguousDisappearance, items: [item("a")])
        model.apply(.questions(questions: model.session.state().questions))
        #expect(model.state.folders.count == 1)
        #expect(model.questionSections.map(\.spaceID) == ["space-documents", "space-gone"])
        #expect(model.questionSectionsShowSpace)
    }

    @Test func noticesStayUntilDismissed() async throws {
        let model = AppModel.fake(scenario: .remoteMassDelete)
        #expect(model.state.questions.isEmpty)
        #expect(model.attentionCount == 0)
        let notice = try #require(model.state.notices.first)

        try #require(model.session.fakeCore()).failSaves(fail: true)
        await #expect(throws: SessionError.NotSaved(reason: "saving is scripted to fail")) {
            try await model.dismiss(notice)
        }
        #expect(model.state.notices == [notice])

        try #require(model.session.fakeCore()).failSaves(fail: false)
        try await model.dismiss(notice)
        #expect(model.state.notices.isEmpty)
        #expect(model.state.problems.isEmpty)
    }

    @Test func aRecoveryWithoutQuestionsShowsOnlyItsNotice() throws {
        let model = AppModel.fake(scenario: .recoveredWithoutQuestions)
        #expect(!model.hasUnseenQuestions)
        #expect(model.questionSections.isEmpty)
        let notice = try #require(model.state.notices.first)
        guard case .recovery(let cause, let heldFiles) = notice.kind else {
            Issue.record("expected a recovery notice")
            return
        }
        #expect(cause.trigger == .spaceRewound)
        #expect(heldFiles == 0)
    }

    @Test func accountActions() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        #expect(try await model.devices().map(\.deviceId) == ["device-1", "device-2"])
        let code = try await model.startPairing()
        #expect(code.code.count == 8)
        try await model.unlinkFolder(spaceID: "space-photos")
        #expect(model.state.folders.map(\.spaceName) == ["Documents"])
        try await model.signOut()
        #expect(model.state.account == nil)
        #expect(model.needsSetup)
        await #expect(throws: SessionError.NotSignedIn) { try await model.devices() }
    }

    @Test func anotherDeviceIsRevokedButNotThisMac() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let devices = try await model.devices()
        await #expect(throws: SessionError.ThisDevice) { try await model.revokeDevice(devices[0]) }
        try await model.revokeDevice(devices[1])
        #expect(try await model.devices().map(\.deviceId) == ["device-1"])
        try await model.revokeDevice(devices[1])
        let unknown = Device(deviceId: "device-9", name: "Mac", platform: "macos", thisDevice: false, addedAtMs: 1)
        await #expect(throws: SessionError.DeviceNotFound) { try await model.revokeDevice(unknown) }
    }

    @Test func theLastDeviceSignsOutOnlyOnceConfirmed() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let fake = try #require(model.session.fakeCore())
        try fake.setDevices(devices: [Device(deviceId: "device-1", name: "Mac", platform: "macos", thisDevice: true, addedAtMs: 1)])
        await #expect(throws: SessionError.LastDevice) { try await model.signOut() }
        #expect(model.state.account != nil)
        try await model.signOut(confirmLastDevice: true)
        #expect(model.state.account == nil)
    }

    private func documents(_ model: AppModel) throws -> URL {
        let folder = try #require(model.state.folders.first { $0.spaceName == "Documents" })
        return URL(filePath: folder.path, directoryHint: .isDirectory)
    }

    @Test func aDeletedFileComesBackOnceItsRestoreIsFinished() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let folder = try documents(model)
        let trash = try await model.trash(in: folder)
        #expect(trash.items.map(\.path) == ["Drafts/Letter to the landlord.docx", "Old Slides", "Q3 Budget.xlsx"])
        #expect(try await model.trash(in: folder.appending(path: "Drafts")).items.count == 1)
        let budget = try #require(trash.items.last)
        #expect(trash.url(of: budget) == folder.appending(path: "Q3 Budget.xlsx"))

        let asked = try await model.restore(budget, from: trash)
        #expect(asked.path == "Q3 Budget.xlsx")
        let pending = try await model.restoreRequests(in: folder)
        #expect(pending.map(\.id) == [asked.requestId])
        #expect(pending.map(\.state.isPending) == [true])

        try #require(model.session.fakeCore()).finishRestore(requestId: asked.requestId, state: .restored)
        let finished = try await model.restoreRequests(in: folder)
        #expect(finished.map(\.state) == [.restored])
        #expect(!finished[0].state.isPending)
        let left = try await model.trash(in: folder)
        #expect(!left.items.contains { $0.id == budget.id })
    }

    @Test func anEarlierVersionIsRestoredAndTheCurrentOneIsRefused() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let file = try documents(model).appending(path: "Reports/Annual Report.docx")
        let versions = try await model.versions(of: file)
        #expect(versions.versions.map(\.current) == [true, false, false])
        #expect(versions.versions.map(\.deviceName) == ["Ada's Mac mini", "Ada's MacBook Air", "Ada's Mac mini"])

        let oldest = try #require(versions.versions.last)
        let asked = try await model.restore(oldest, of: file)
        #expect(asked.path == "Reports/Annual Report.docx")
        let requests = try await model.restoreRequests(in: file.deletingLastPathComponent())
        #expect(requests.map(\.target) == [.version(nodeId: versions.nodeId, versionId: oldest.versionId, savedAtMs: oldest.savedAtMs)])
        await #expect(throws: SessionError.VersionIsCurrent) { try await model.restore(versions.versions[0], of: file) }
    }

    @Test func historyOutsideALinkedFolderIsRefused() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let elsewhere = FileManager.default.temporaryDirectory.appending(path: "Elsewhere")
        let refused = SessionError.PathNotInLinkedFolder(path: elsewhere.path(percentEncoded: false))
        await #expect(throws: refused) { try await model.trash(in: elsewhere) }
        await #expect(throws: refused) { try await model.restoreRequests(in: elsewhere) }
        let folder = try documents(model)
        await #expect(throws: SessionError.PathIsLinkedFolder) { try await model.versions(of: folder) }
    }

    @Test func aNewLinkIsListedWithoutItsURLAndARevokedOneStaysListed() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let seeded = try await model.shareLinks()
        #expect(seeded.map(\.state) == [.active, .active, .active, .expired, .revoked])
        #expect(seeded.filter { $0.itemURL == nil }.count == 1)

        let file = try documents(model).appending(path: "Reports/Q3.docx")
        let new = try await model.createShareLink(for: file, expiry: .day, password: "eight888")
        #expect(new.url.hasPrefix("https://box.example.com/s/"))
        #expect(new.link.itemURL == file)
        #expect(new.link.hasPassword)
        let listed = try await model.shareLinks()
        #expect(listed.first == new.link)
        #expect(listed.count == seeded.count + 1)

        let revoked = try await model.revoke(new.link)
        #expect(revoked.id == new.link.id)
        #expect(revoked.state == .revoked)
        #expect(try await model.shareLinks().first?.state == .revoked)
    }

    @Test func aShareLinkIsRefusedOutsideALinkedFolderOrWithAShortPassword() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        let elsewhere = FileManager.default.temporaryDirectory.appending(path: "Elsewhere/a.docx")
        let refused = SessionError.PathNotInLinkedFolder(path: elsewhere.path(percentEncoded: false))
        await #expect(throws: refused) { try await model.createShareLink(for: elsewhere, expiry: .week, password: nil) }
        let file = try documents(model).appending(path: "Notes.txt")
        await #expect(throws: SessionError.PasswordLength) {
            try await model.createShareLink(for: file, expiry: .month, password: "short")
        }
    }
}

@MainActor
@Suite struct SetupModelTests {
    @Test func claimAServerAndLinkANewSpace() async throws {
        let folder = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: folder) }
        let app = AppModel.fake(scenario: .firstRun)
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Ada's Mac")
        #expect(!setup.canConnect)

        setup.serverURL = "box.example.com"
        await setup.connect()
        #expect(setup.error == .InvalidServerUrl)
        #expect(setup.step == .server)

        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.step == .signIn)
        #expect(setup.claimsServer)

        setup.code = "ABCD-EFGH-JKLM"
        #expect(!setup.canSignIn)
        setup.userName = "Ada"
        #expect(setup.canSignIn)
        await setup.signIn()
        #expect(setup.error == nil)
        #expect(setup.step == .folder)
        #expect(setup.spaces.isEmpty)
        #expect(setup.selectedSpaceID == nil)

        #expect(!setup.canLink)
        setup.folderPath = folder.path(percentEncoded: false)
        #expect(setup.canLink)
        await setup.link()
        #expect(setup.error == nil)
        #expect(setup.step == .done)
        #expect(app.state.folders.map(\.spaceName) == ["Koidrive"])
        #expect(app.state.status == .idle)
        #expect(!app.needsSetup)
    }

    @Test func pairWithAClaimedServerAndLinkAnExistingSpace() async throws {
        let folder = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: folder) }
        let app = AppModel.fake(scenario: .firstRun)
        try #require(app.session.fakeCore()).setServerClaimed(claimed: true)
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(!setup.claimsServer)

        setup.code = "WRONG"
        await setup.signIn()
        #expect(setup.error == .InvalidCode)
        #expect(setup.step == .signIn)

        setup.code = "ABCD-EFGH"
        await setup.signIn()
        #expect(setup.step == .folder)
        let shared = try #require(setup.spaces.first)
        #expect(setup.selectedSpaceID == shared.spaceId)

        setup.folderPath = folder.appending(path: "missing").path(percentEncoded: false)
        await setup.link()
        #expect(setup.error == .FolderNotFound)
        setup.folderPath = folder.path(percentEncoded: false)
        await setup.link()
        #expect(setup.step == .done)
        #expect(app.state.folders.map(\.spaceId) == [shared.spaceId])
    }

    @Test func serverErrorsKeepTheStep() async throws {
        let app = AppModel.fake(scenario: .firstRun)
        try #require(app.session.fakeCore()).failNext(error: .ServerUnreachable(reason: "timed out"))
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.error == .ServerUnreachable(reason: "timed out"))
        #expect(setup.step == .server)
        await setup.connect()
        #expect(setup.step == .signIn)
        setup.changeServer()
        #expect(setup.step == .server)
    }

    @Test func addAFolderToAConfiguredDeviceFinishesWhenTheLinkSucceeds() async throws {
        let folder = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: folder) }
        let app = AppModel.fake(scenario: .upToDate)
        app.start()
        let linked = app.state.folders.map(\.spaceId)
        try #require(!linked.isEmpty)
        let setup = SetupModel(app: app, mode: .addFolder, deviceName: "Mac")
        #expect(setup.step == .folder)

        await setup.loadSpaces()
        setup.selectedSpaceID = nil
        setup.newSpaceName = "Projects"
        setup.folderPath = folder.appending(path: "missing").path(percentEncoded: false)
        await setup.link()
        #expect(setup.error == .FolderNotFound)
        #expect(setup.step == .folder)

        setup.folderPath = folder.path(percentEncoded: false)
        await setup.link()
        #expect(setup.error == nil)
        #expect(setup.step == .done)
        #expect(app.state.folders.count == linked.count + 1)
        #expect(app.state.folders.filter { $0.spaceName == "Projects" }.count == 1)
        #expect(setup.spaces.filter { $0.name == "Projects" }.count == 1)
    }

    @Test func anUndecidedSetupAsksAndConnectStoresUsageReportsOn() async throws {
        let app = AppModel.fake(scenario: .firstRun)
        try #require(app.session.fakeCore()).failNext(error: .ServerUnreachable(reason: "timed out"))
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        #expect(setup.asksForUsageReports)
        #expect(setup.sendsUsageReports)

        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.step == .server)
        #expect(!app.telemetry.decided)

        await setup.connect()
        #expect(setup.step == .signIn)
        #expect(app.telemetry == TelemetrySetting(enabled: true, decided: true, locked: false))
        #expect(!setup.asksForUsageReports)
    }

    @Test func usageReportsTurnedOffInSetupAreStoredOffOnConnect() async throws {
        let app = AppModel.fake(scenario: .firstRun)
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        setup.sendsUsageReports = false
        setup.sendsUsageReports = true
        setup.sendsUsageReports = false
        #expect(!app.telemetry.decided)

        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.step == .signIn)
        #expect(app.telemetry == TelemetrySetting(enabled: false, decided: true, locked: false))
    }

    @Test func aDecidedSetupDoesNotAskOrChangeUsageReports() async throws {
        let app = AppModel.fake(scenario: .firstRun)
        try #require(app.session.fakeCore()).setTelemetrySetting(
            setting: TelemetrySetting(enabled: false, decided: true, locked: false))
        app.start()
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        #expect(!setup.asksForUsageReports)

        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.step == .signIn)
        #expect(app.telemetry == TelemetrySetting(enabled: false, decided: true, locked: false))
    }

    @Test func aLockedSetupDoesNotAskAndLeavesUsageReportsUndecided() async throws {
        let app = AppModel.fake(scenario: .firstRun)
        let locked = TelemetrySetting(enabled: false, decided: false, locked: true)
        try #require(app.session.fakeCore()).setTelemetrySetting(setting: locked)
        app.start()
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        #expect(!setup.asksForUsageReports)

        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.step == .signIn)
        #expect(app.telemetry == locked)
    }

    @Test func aHiddenChoiceIsNotStoredWhenTheSettingBecomesUndecidedDuringConnect() async throws {
        let app = AppModel.fake(scenario: .firstRun)
        let fake = try #require(app.session.fakeCore())
        fake.setTelemetrySetting(setting: TelemetrySetting(enabled: false, decided: true, locked: false))
        app.start()
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        #expect(!setup.asksForUsageReports)

        let undecided = TelemetrySetting(enabled: false, decided: false, locked: false)
        fake.setTelemetrySetting(setting: undecided)
        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.step == .signIn)
        #expect(app.telemetry == undecided)
    }

    @Test func aChoiceTheCLIMakesDuringConnectIsKept() async throws {
        let app = AppModel.fake(scenario: .firstRun)
        let fake = try #require(app.session.fakeCore())
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        #expect(setup.asksForUsageReports)
        #expect(setup.sendsUsageReports)

        let cli = TelemetrySetting(enabled: false, decided: true, locked: false)
        fake.setTelemetrySetting(setting: cli)
        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.step == .signIn)
        #expect(app.telemetry == cli)
    }

    @Test func incompleteStepsMakeNoCall() async throws {
        let app = AppModel.fake(scenario: .firstRun)
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        await setup.connect()
        #expect(setup.error == nil)
        #expect(setup.step == .server)

        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.step == .signIn)
        await setup.signIn()
        #expect(setup.error == nil)
        #expect(setup.step == .signIn)
        await setup.link()
        #expect(setup.error == nil)
        #expect(app.state.account == nil)
    }
}
