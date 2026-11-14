// SPDX-License-Identifier: Apache-2.0

#if DEBUG
import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

@MainActor
enum Snapshots {
    static var directory: URL? {
        UserDefaults.standard.string(forKey: "RenderSnapshots").map { URL(filePath: $0, directoryHint: .isDirectory) }
    }

    static func render(into directory: URL) async {
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        for dark in [false, true] {
            let suffix = dark ? "dark" : "light"
            for (name, model) in await menuStates() {
                await capture(MenuContent().environment(model).background(.background), width: 330, titled: nil,
                              dark: dark, to: directory.appending(path: "menubar-\(name)-\(suffix).png"))
            }
            for (name, setup, app, afterAppearing) in await setupStates() {
                await capture(SetupSteps(setup: setup).environment(app).frame(width: 480), width: 480,
                              titled: "Set Up Koidrive", dark: dark, to: directory.appending(path: "setup-\(name)-\(suffix).png"),
                              afterAppearing: afterAppearing)
            }
            for (name, scenario) in [("mass-delete", FakeScenario.massDelete), ("recovery", .recovery),
                                     ("ambiguous", .ambiguousDisappearance), ("two-spaces", .manyQuestions),
                                     ("none", .upToDate)] {
                let model = AppModel.preview(scenario)
                await capture(QuestionsView().environment(model).frame(width: 860, height: 560), width: 860, height: 560, titled: "Needs You",
                              dark: dark, to: directory.appending(path: "questions-\(name)-\(suffix).png"))
            }
            let oneSpace = AppModel.preview(.massDelete)
            try? oneSpace.session.unlinkFolder(spaceId: "space-photos")
            await capture(QuestionsView().environment(oneSpace).frame(width: 860, height: 560), width: 860, height: 560,
                          titled: "Needs You", dark: dark, to: directory.appending(path: "questions-one-space-\(suffix).png"))
            for (name, scenario, error, answering) in [("answer-failed", FakeScenario.ambiguousDisappearance, SessionError.QuestionNotFound, false),
                                                       ("sending", .massDelete, nil, true)] {
                let model = AppModel.preview(scenario)
                if let question = model.state.questions.first {
                    await capture(QuestionDetail(question: question, folder: model.folder(spaceID: question.spaceId), error: error, answering: answering)
                        .environment(model).background(Color.panel).frame(width: 600, height: 528), width: 600, height: 528, titled: nil,
                                  dark: dark, to: directory.appending(path: "questions-detail-\(name)-\(suffix).png"))
                }
            }
            let mass = AppModel.preview(.massDelete)
            if let question = mass.state.questions.first {
                await capture(ConfirmSheet(title: question.confirmation(for: .delete),
                                           message: question.confirmationMessage(for: .delete),
                                           action: question.title(for: .delete), confirm: {}, cancel: {}),
                              width: 280, titled: nil, dark: dark,
                              to: directory.appending(path: "questions-confirm-\(suffix).png"))
            }
            await capture(ButtonGallery(), width: 432, titled: nil, dark: dark,
                          to: directory.appending(path: "buttons-\(suffix).png"))
            for (name, view) in await settingsStates() {
                await capture(view.frame(width: 600, height: 380), width: 600, height: 380, titled: nil, dark: dark,
                              to: directory.appending(path: "settings-\(name)-\(suffix).png"))
            }
            for (name, view) in await deletedFilesStates() {
                await capture(view.frame(width: 880, height: 600), width: 880, height: 600, titled: "Deleted Files", dark: dark,
                              to: directory.appending(path: "deleted-files-\(name)-\(suffix).png"))
            }
            for (name, view) in await versionHistoryStates() {
                await capture(view.frame(width: 720, height: 480), width: 720, height: 480, titled: "Version History", dark: dark,
                              to: directory.appending(path: "version-history-\(name)-\(suffix).png"))
            }
            for (name, targeted) in [("1-drop", DropTarget?.none), ("2-drop-on-version-history", .versionHistory)] {
                await capture(DropPanel(targets: DropTarget.allCases, targeted: targeted) { _, _ in }.padding(12), width: 354, titled: nil,
                              dark: dark, to: directory.appending(path: "drop-panel-\(name)-\(suffix).png"))
            }
            await captureOpenPanel(dark: dark, to: directory.appending(path: "version-history-0-file-picker-\(suffix).png"))
        }
    }

    private static func deletedFilesStates() async -> [(String, AnyView)] {
        func make(_ scenario: FakeScenario = .upToDate) -> (AppModel, DeletedFilesModel) {
            let app = AppModel.preview(scenario)
            if let fake = app.session.fakeCore(), let documents = app.state.folders.first, let photos = app.state.folders.last {
                _ = try? fake.setTrash(spaceId: documents.spaceId, items: [
                    deletedItem("node-archive", "Archive", .folder, files: 2, bytes: 14_200_000, at: day(-3, 14, 32), by: "device-3", "Studio iMac"),
                    deletedItem("node-launch", "Campaigns/Launch assets", .folder, files: 8, bytes: 12_600_000, at: day(-2, 16, 18),
                                by: "device-3", "Studio iMac"),
                    deletedItem("node-budget", "Finance/Budget 2026.numbers", .file, files: 1, bytes: 248_000, at: day(0, 9, 41),
                                by: "device-2", "Ada's MacBook Air"),
                    deletedItem("node-notes", "Notes/Notes.md", .file, files: 1, bytes: 4_000, at: day(-3, 9, 6), by: "device-1", "Ada's Mac mini"),
                ])
                _ = try? fake.setTrash(spaceId: photos.spaceId, items: [
                    deletedItem("node-trip", "2026/Trip drafts", .folder, files: 18, bytes: 96_000_000, at: day(0, 8, 12), by: "device-1", "Ada's Mac mini"),
                    deletedItem("node-0412", "2026/Seoul/IMG_0412.heic", .file, files: 1, bytes: 3_100_000, at: day(0, 7, 55),
                                by: "device-1", "Ada's Mac mini"),
                    deletedItem("node-0388", "2026/Seoul/IMG_0388.heic", .file, files: 1, bytes: 2_800_000, at: day(-5, 18, 20),
                                by: "device-2", "Ada's MacBook Air"),
                ])
            }
            return (app, DeletedFilesModel(app: app))
        }
        func view(_ app: AppModel, _ files: DeletedFilesModel, loads: Bool = true) -> AnyView {
            AnyView(DeletedFilesView(files: files, loadsOnAppear: loads).environment(app))
        }
        func select(_ files: DeletedFilesModel, _ nodeID: String) {
            files.selection = files.sections.flatMap(\.entries).first { $0.item.nodeId == nodeID }?.id
        }

        let (all, allFiles) = make()
        await allFiles.load()
        select(allFiles, "node-budget")

        let (requested, requestedFiles) = make()
        await requestedFiles.load()
        select(requestedFiles, "node-budget")
        await requestedFiles.restoreSelected()

        let (photos, photosFiles) = make()
        await photosFiles.load()
        photosFiles.scope = .folder(spaceID: photos.state.folders.last?.spaceId ?? "")
        select(photosFiles, "node-0412")

        let (restores, restoresFiles) = make()
        await restoresFiles.load()
        if let fake = restores.session.fakeCore(), let documents = restores.state.folders.first {
            let plan = documents.url.appending(path: "Launch plan.key")
            _ = try? fake.setVersions(spaceId: documents.spaceId, versions: FileVersions(path: "Launch plan.key", nodeId: "node-plan", versions: [
                FileVersion(versionId: "v2", size: 6_400_000, savedAtMs: day(0, 9, 41), deviceId: "device-1", deviceName: "Ada's Mac mini", current: true),
                FileVersion(versionId: "v1", size: 5_900_000, savedAtMs: day(-3, 9, 6), deviceId: "device-1", deviceName: "Ada's Mac mini", current: false),
            ]))
            var asked: [String: String] = [:]
            for nodeID in ["node-budget", "node-notes", "node-archive", "node-launch", "node-trip", "node-0388"] {
                select(restoresFiles, nodeID)
                await restoresFiles.restoreSelected()
                asked[nodeID] = restoresFiles.scopedFolders.flatMap(\.requests).first { $0.target == .deleted(nodeId: nodeID) }?.id
            }
            if let versions = try? await restores.versions(of: plan), let earlier = versions.versions.last,
               let version = try? await restores.restore(earlier, of: plan) {
                _ = try? fake.finishRestore(requestId: version.requestId, state: .restored)
            }
            let ends: [(String, RestoreState)] = [
                ("node-archive", .restoredAs(name: "Archive 2")), ("node-launch", .refused(reason: .gone)),
                ("node-0388", .restoredAsCopy(nodeId: "node-copy", name: "IMG_0388 2.heic")), ("node-trip", .tooLarge),
                ("node-notes", .pending(heldByProblem: true)),
            ]
            for (nodeID, state) in ends {
                if let id = asked[nodeID] { _ = try? fake.finishRestore(requestId: id, state: state) }
            }
            await restoresFiles.refreshRequests()
        }
        restoresFiles.tab = .restores

        let (noRestores, noRestoresFiles) = make()
        await noRestoresFiles.load()
        noRestoresFiles.tab = .restores

        let empty = AppModel.preview(.syncing)
        let emptyFiles = DeletedFilesModel(app: empty)

        let nothing = AppModel.preview(.upToDate)
        let nothingFiles = DeletedFilesModel(app: nothing)
        nothingFiles.scope = .folder(spaceID: nothing.state.folders.last?.spaceId ?? "")

        let (loading, loadingFiles) = make()

        let (error, errorFiles) = make()
        error.session.fakeCore()?.failNext(error: .Failed(reason: "The server didn’t answer."))

        let (offline, offlineFiles) = make()
        offline.session.fakeCore()?.setActivity(activity: .offline)
        offline.session.fakeCore()?.failNext(error: .ServerUnreachable(reason: "offline"))

        let (search, searchFiles) = make()
        searchFiles.search = "invoice"

        try? await Task.sleep(for: .milliseconds(200))
        return [
            ("1-all-folders", view(all, allFiles)), ("2-restore-requested", view(requested, requestedFiles)),
            ("3-one-folder", view(photos, photosFiles)), ("4-restores", view(restores, restoresFiles)),
            ("5-no-restores", view(noRestores, noRestoresFiles)), ("6-empty", view(empty, emptyFiles)),
            ("7-nothing-in-photos", view(nothing, nothingFiles)), ("8-loading", view(loading, loadingFiles, loads: false)),
            ("9-error", view(error, errorFiles)), ("10-offline", view(offline, offlineFiles)),
            ("11-no-matches", view(search, searchFiles)),
        ]
    }

    private static func versionHistoryStates() async -> [(String, AnyView)] {
        let versions = [
            FileVersion(versionId: "v4", size: 6_400_000, savedAtMs: day(0, 9, 41), deviceId: "device-1", deviceName: "Ada's Mac mini", current: true),
            FileVersion(versionId: "v3", size: 6_200_000, savedAtMs: day(-1, 16, 18), deviceId: "device-2", deviceName: "Ada's MacBook Air", current: false),
            FileVersion(versionId: "v2", size: 5_900_000, savedAtMs: day(-3, 9, 6), deviceId: "device-1", deviceName: "Ada's Mac mini", current: false),
            FileVersion(versionId: "v1", size: 5_100_000, savedAtMs: day(-4, 14, 32), deviceId: "device-3", deviceName: "Studio iMac", current: false),
        ]
        func make(_ kept: [FileVersion]? = versions) -> (AppModel, VersionHistoryModel) {
            let app = AppModel.preview(.upToDate)
            let documents = app.state.folders.first
            if let kept, let documents {
                _ = try? app.session.fakeCore()?.setVersions(spaceId: documents.spaceId, versions: FileVersions(
                    path: "Launch plan.key", nodeId: "node-plan", versions: kept))
            }
            let file = (documents?.url ?? URL(filePath: "/")).appending(path: "Launch plan.key")
            return (app, VersionHistoryModel(app: app, file: file))
        }
        func view(_ app: AppModel, _ history: VersionHistoryModel, loads: Bool = true) -> AnyView {
            AnyView(VersionHistoryView(history: history, loadsOnAppear: loads).environment(app))
        }
        func requested() async -> (AppModel, VersionHistoryModel) {
            let (app, history) = make()
            await history.load()
            history.selection = "v3"
            await history.restoreSelected()
            return (app, history)
        }

        let (list, listHistory) = make()
        await listHistory.load()
        listHistory.selection = "v3"

        let (asked, askedHistory) = await requested()

        let (restored, restoredHistory) = await requested()
        if case .requested(_, let id) = restoredHistory.restore, let fake = restored.session.fakeCore(),
           let documents = restored.state.folders.first {
            let now = FileVersion(versionId: "v5", size: 6_200_000, savedAtMs: Int64(Date.now.timeIntervalSince1970 * 1000),
                                  deviceId: "device-1", deviceName: "Ada's Mac mini", current: true)
            let earlier = versions.map { FileVersion(versionId: $0.versionId, size: $0.size, savedAtMs: $0.savedAtMs,
                                                     deviceId: $0.deviceId, deviceName: $0.deviceName, current: false) }
            _ = try? fake.setVersions(spaceId: documents.spaceId, versions: FileVersions(path: "Launch plan.key", nodeId: "node-plan",
                                                                                     versions: [now] + earlier))
            _ = try? fake.finishRestore(requestId: id, state: .restored)
            await restoredHistory.refreshRestore()
        }

        let (refused, refusedHistory) = await requested()
        if case .requested(_, let id) = refusedHistory.restore {
            _ = try? refused.session.fakeCore()?.finishRestore(requestId: id, state: .refused(reason: .gone))
            await refusedHistory.refreshRestore()
        }

        let (notSynced, notSyncedHistory) = make(nil)
        let (single, singleHistory) = make(Array(versions.prefix(1)))
        let (loading, loadingHistory) = make()
        let (error, errorHistory) = make()
        error.session.fakeCore()?.failNext(error: .Failed(reason: "The server didn’t answer."))
        let (offline, offlineHistory) = make()
        offline.session.fakeCore()?.setActivity(activity: .offline)
        offline.session.fakeCore()?.failNext(error: .ServerUnreachable(reason: "offline"))

        try? await Task.sleep(for: .milliseconds(200))
        return [
            ("1-versions", view(list, listHistory)), ("2-restore-requested", view(asked, askedHistory)),
            ("3-restored", view(restored, restoredHistory)), ("4-refused", view(refused, refusedHistory)),
            ("5-not-synced-yet", view(notSynced, notSyncedHistory)), ("6-no-earlier-versions", view(single, singleHistory)),
            ("7-loading", view(loading, loadingHistory, loads: false)), ("8-error", view(error, errorHistory)),
            ("9-offline", view(offline, offlineHistory)),
        ]
    }

    private static func captureOpenPanel(dark: Bool, to url: URL) async {
        let root = FileManager.default.temporaryDirectory.appending(path: "Koidrive Snapshots", directoryHint: .isDirectory)
        let folders = ["Documents", "Photos"].map { name in
            LinkedFolder(spaceId: "space-\(name)", spaceName: name, path: root.appending(path: name).path(percentEncoded: false), state: .upToDate)
        }
        for path in ["Documents/Archive", "Documents/Finance", "Photos/2026"] {
            try? FileManager.default.createDirectory(at: root.appending(path: path), withIntermediateDirectories: true)
        }
        for path in ["Documents/Launch plan.key", "Documents/Contract draft.pdf", "Documents/Budget 2026.numbers", "Documents/Notes.md"] {
            FileManager.default.createFile(atPath: root.appending(path: path).path(percentEncoded: false), contents: Data(path.utf8))
        }
        let (panel, filter) = VersionHistoryPicker.makePanel(in: folders)
        panel.appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
        panel.setContentSize(NSSize(width: 720, height: 420))
        panel.begin { _ in withExtendedLifetime(filter) {} }
        try? await Task.sleep(for: .seconds(1.5))
        shoot(panel, to: url)
        panel.cancel(nil)
    }

    private static func day(_ offset: Int, _ hour: Int, _ minute: Int) -> Int64 {
        let calendar = Calendar.current
        let start = calendar.date(byAdding: .day, value: offset, to: calendar.startOfDay(for: .now)) ?? .now
        let date = calendar.date(bySettingHour: hour, minute: minute, second: 0, of: start) ?? start
        return Int64(date.timeIntervalSince1970 * 1000)
    }

    private static func deletedItem(_ id: String, _ path: String, _ kind: NodeKind, files: UInt64, bytes: UInt64, at ms: Int64,
                                    by device: String, _ name: String) -> DeletedItem {
        DeletedItem(nodeId: id, path: path, kind: kind, files: files, bytes: bytes, modifiedAtMs: ms - 3_600_000,
                    deletedAtMs: ms, deletedByDeviceId: device, deletedByDeviceName: name)
    }

    private static func menuStates() async -> [(String, AppModel)] {
        var states: [(String, AppModel)] = [
            ("1-up-to-date", .preview(.upToDate)), ("2-syncing", .preview(.syncing)), ("3-paused", .preview(.paused)),
            ("4-offline", .preview(.offline)),
        ]
        let question = AppModel.preview(.massDelete)
        if let fake = question.session.fakeCore(), let folder = question.state.folders.last {
            _ = try? fake.notify(spaceId: folder.spaceId, kind: .remoteMassDelete(
                deletions: 214, affectedFiles: 1_086, spaceFiles: 1_840, exceeded: [.shareOfSpace(percent: 50)]))
        }
        states.append(("5-question", question))
        states.append(("5b-recovery", .preview(.recovery)))
        let problem = AppModel.preview(.upToDate)
        if let fake = problem.session.fakeCore(), let folder = problem.state.folders.last {
            _ = try? fake.reportProblem(spaceId: folder.spaceId, path: nil, kind: .folderMissing)
            _ = try? fake.setFolderState(spaceId: folder.spaceId, folderState: .halted)
        }
        states.append(("6-problem", problem))
        states.append(("7-first-run", .preview(.firstRun)))
        let connecting = AppModel.preview(.upToDate)
        connecting.session.fakeCore()?.setActivity(activity: .connecting)
        states.append(("8-connecting", connecting))
        let stopped = AppModel.preview(.upToDate)
        stopped.stop()
        states.append(("9-stopped", stopped))
        try? await Task.sleep(for: .milliseconds(200))
        return states
    }

    private static func setupStates() async -> [(String, SetupModel, AppModel, (@MainActor () async -> Void)?)] {
        func start(claimed: Bool) -> (SetupModel, AppModel) {
            let app = AppModel.preview(.firstRun)
            try? app.session.fakeCore()?.setServerClaimed(claimed: claimed)
            return (SetupModel(app: app, mode: .firstRun, deviceName: "Gunwoo's MacBook Pro"), app)
        }
        let (server, serverApp) = start(claimed: false)
        server.serverURL = "https://koi.example.com"

        let (unreachable, unreachableApp) = start(claimed: false)
        unreachable.serverURL = "https://koi.exmaple.com"
        unreachableApp.session.fakeCore()?.failNext(error: .ServerUnreachable(reason: "A server with the specified hostname could not be found."))
        await unreachable.connect()

        let decidedApp = AppModel.preview(.firstRun, telemetry: TelemetrySetting(enabled: true, decided: true, locked: false))
        let decided = SetupModel(app: decidedApp, mode: .firstRun, deviceName: "Gunwoo's MacBook Pro")
        decided.serverURL = "https://koi.example.com"

        let (pair, pairApp) = start(claimed: true)
        pair.serverURL = "https://koi.example.com"
        await pair.connect()
        pair.code = "K7Q4-M2XD"
        pairApp.session.fakeCore()?.failNext(error: .InvalidCode)
        await pair.signIn()

        let (pairing, pairingApp) = start(claimed: true)
        pairing.serverURL = "https://koi.example.com"
        await pairing.connect()
        pairing.code = "K7Q4-M2XD"

        let (claim, claimApp) = start(claimed: false)
        claim.serverURL = "https://koi.example.com"
        await claim.connect()
        claim.userName = "Gunwoo"
        claim.code = "R3VN-8KPT-Q2ZL"

        let (folder, folderApp) = start(claimed: false)
        folder.serverURL = "https://koi.example.com"
        await folder.connect()
        folder.userName = "Gunwoo"
        folder.code = "R3VN-8KPT-Q2ZL"
        await folder.signIn()
        folder.newSpaceName = "Projects"
        let projects = FileManager.default.temporaryDirectory.appending(path: "Projects", directoryHint: .isDirectory)
        try? FileManager.default.createDirectory(at: projects, withIntermediateDirectories: true)
        folder.folderPath = projects.path(percentEncoded: false)

        func addFolder() -> (SetupModel, AppModel) {
            let app = AppModel.preview(.upToDate)
            return (SetupModel(app: app, mode: .addFolder, deviceName: "Gunwoo's MacBook Pro"), app)
        }
        let documents = FileManager.default.temporaryDirectory.appending(path: "Documents", directoryHint: .isDirectory)
        try? FileManager.default.createDirectory(at: documents, withIntermediateDirectories: true)
        let (existing, existingApp) = addFolder()
        await existing.loadSpaces()
        existing.folderPath = documents.path(percentEncoded: false)
        let (noFolder, noFolderApp) = addFolder()
        await noFolder.loadSpaces()
        let (overlap, overlapApp) = addFolder()
        await overlap.loadSpaces()
        overlap.folderPath = documents.path(percentEncoded: false)
        let failLinking: @MainActor () async -> Void = {
            overlapApp.session.fakeCore()?.failNext(error: .FolderOverlaps(other: "~/Koidrive/Photos"))
            await overlap.link()
        }

        let (done, doneApp) = start(claimed: false)
        done.serverURL = "https://koi.example.com"
        await done.connect()
        done.userName = "Gunwoo"
        done.code = "R3VN-8KPT-Q2ZL"
        await done.signIn()
        done.newSpaceName = "Projects"
        done.folderPath = projects.path(percentEncoded: false)
        await done.link()

        return [("1-server", server, serverApp, nil), ("1a-server-error", unreachable, unreachableApp, nil),
                ("1b-server-decided", decided, decidedApp, nil), ("2-sign-in", pairing, pairingApp, nil),
                ("2a-sign-in-error", pair, pairApp, nil), ("2b-claim", claim, claimApp, nil),
                ("3-folder", folder, folderApp, nil), ("3a-folder-existing", existing, existingApp, nil),
                ("3b-folder-none", noFolder, noFolderApp, nil), ("3c-folder-overlap", overlap, overlapApp, failLinking),
                ("4-done", done, doneApp, nil)]
    }

    private static func settingsStates() async -> [(String, AnyView)] {
        let folders = AppModel.preview(.upToDate)
        if let fake = folders.session.fakeCore(), let folder = folders.state.folders.last {
            _ = try? fake.setFolderState(spaceId: folder.spaceId, folderState: .halted)
        }
        let syncing = AppModel.preview(.upToDate)
        if let fake = syncing.session.fakeCore(), let folder = syncing.state.folders.first {
            _ = try? fake.setFolderState(spaceId: folder.spaceId, folderState: .syncing)
        }
        let pairing = PairingCode(code: "K7Q4-M2XD", expiresAtMs: Int64(Date.now.addingTimeInterval(600).timeIntervalSince1970 * 1000))
        return [
            ("1-folders", AnyView(FoldersSettings().environment(folders))),
            ("1b-folders-empty", AnyView(FoldersSettings().environment(AppModel.preview(.firstRun)))),
            ("1c-folders-syncing", AnyView(FoldersSettings().environment(syncing))),
            ("2-account", AnyView(AccountSettings(pairing: pairing).environment(AppModel.preview(.upToDate)))),
            ("3-problems", AnyView(ProblemsSettings().environment(AppModel.preview(.problems)))),
            ("4-general", AnyView(GeneralSettings().environment(AppModel.preview(.upToDate)))),
            ("4b-general-usage-reports-off", AnyView(GeneralSettings().environment(
                AppModel.preview(.upToDate, telemetry: TelemetrySetting(enabled: false, decided: true, locked: false)))
            )),
            ("4c-general-usage-reports-locked", AnyView(GeneralSettings().environment(
                AppModel.preview(.upToDate, telemetry: TelemetrySetting(enabled: false, decided: false, locked: true)))
            )),
            ("5a-storage-one-space", AnyView(StorageSettings().environment(AppModel.previewWithOneSpace()))),
            ("5b-storage-two-spaces", AnyView(StorageSettings().environment(AppModel.preview(.upToDate)))),
        ]
    }

    private static func capture(_ view: some View, width: CGFloat, height: CGFloat? = nil, titled: String?,
                                dark: Bool, to url: URL, afterAppearing: (@MainActor () async -> Void)? = nil) async {
        let appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
        let host = NSHostingView(rootView: view
            .buttonStyle(.secondary)
            .background(Color(nsColor: .windowBackgroundColor))
            .environment(\.colorScheme, dark ? .dark : .light))
        host.appearance = appearance
        if height != nil {
            host.sizingOptions = [.minSize]
        }
        let style: NSWindow.StyleMask = titled == nil ? [.borderless] : [.titled, .closable, .miniaturizable]
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: width, height: height ?? 400), styleMask: style,
                              backing: .buffered, defer: false)
        window.appearance = appearance
        window.title = titled ?? ""
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderFrontRegardless()
        for _ in 0..<3 {
            try? await Task.sleep(for: .milliseconds(250))
            let size = NSSize(width: width, height: height ?? host.fittingSize.height)
            if window.contentRect(forFrameRect: window.frame).size != size {
                window.setContentSize(size)
            }
        }
        if let afterAppearing {
            await afterAppearing()
        }
        try? await Task.sleep(for: .milliseconds(300))
        shoot(window, to: url)
        window.close()
    }

    private static func shoot(_ window: NSWindow, to url: URL) {
        let screencapture = Process()
        screencapture.executableURL = URL(filePath: "/usr/sbin/screencapture")
        screencapture.arguments = ["-x", "-o", "-l", String(window.windowNumber), url.path(percentEncoded: false)]
        do {
            try screencapture.run()
        } catch {
            fatalError("screencapture could not start for \(url.lastPathComponent): \(error)")
        }
        screencapture.waitUntilExit()
        guard screencapture.terminationStatus == 0 else {
            fatalError("screencapture failed for \(url.lastPathComponent) with status \(screencapture.terminationStatus)")
        }
    }
}

private struct ButtonGallery: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            Button("Restore Version…") {}
                .buttonStyle(.primary)
            Button("Cancel") {}
            HStack(spacing: 12) {
                Button("Show in Finder") {}
                Button("Remove…", role: .destructive) {}
            }
            .controlSize(.small)
            HStack(spacing: 12) {
                Button("Remove Device") {}
                    .buttonStyle(.destructiveConfirm)
                Button("Remove…", role: .destructive) {}
            }
            HStack(spacing: 12) {
                Button("Restore Version…") {}
                    .buttonStyle(.primary)
                Button("Cancel") {}
            }
            .disabled(true)
            HStack(spacing: 8) {
                Spacer()
                Button("Cancel") {}
                Button("Restore Version…") {}
                    .buttonStyle(.primary)
            }
        }
        .padding(24)
    }
}
#endif
