// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

@main
struct KoidriveApp: App {
    @State private var model: AppModel
    private let launchWindow = AppModel.launchWindow(in: .standard)

    init() {
        if CommandLine.arguments.dropFirst().first == "--unregister-login-item" {
            do {
                try LoginItemCommand.unregister()
                exit(0)
            } catch {
                FileHandle.standardError.write(Data("Could not unregister Koidrive's login item: \(error.localizedDescription)\n".utf8))
                exit(1)
            }
        }
        let model: AppModel
        #if DEBUG
        if let directory = Snapshots.directory {
            model = AppModel.fake(scenario: .upToDate)
            Task {
                await Snapshots.render(into: directory)
                exit(0)
            }
            _model = State(initialValue: model)
            return
        }
        #endif
        if let scenario = AppModel.launchScenario(in: .standard) {
            model = AppModel.fake(scenario: scenario)
        } else {
            let helper = Bundle.main.bundleURL.appending(path: "Contents/Helpers/koi")
            do {
                model = try AppModel.open(helper: helper)
            } catch {
                Self.quit(after: error)
            }
        }
        model.start()
        _model = State(initialValue: model)
    }

    private static func quit(after error: any Error) -> Never {
        let alert = NSAlert()
        alert.messageText = String(localized: "Koidrive can't start")
        alert.informativeText = (error as? SessionError)?.message ?? error.localizedDescription
        alert.runModal()
        exit(1)
    }

    var body: some Scene {
        MenuBarExtra {
            MenuContent()
                .environment(model)
                .buttonStyle(.secondary)
        } label: {
            MenuBarLabel(model: model)
                .modifier(MenuBarDrop(model: model))
        }
        .menuBarExtraStyle(.window)

        Window("Set Up Koidrive", id: WindowID.setup) {
            SetupView()
                .environment(model)
                .buttonStyle(.secondary)
        }
        .windowResizability(.contentSize)

        Window("Needs You", id: WindowID.questions) {
            QuestionsView()
                .environment(model)
                .buttonStyle(.secondary)
        }
        .defaultSize(width: 860, height: 560)

        Window("Deleted Files", id: WindowID.deletedFiles) {
            DeletedFilesView(app: model)
                .environment(model)
                .buttonStyle(.secondary)
        }
        .defaultSize(width: 880, height: 600)
        .defaultLaunchBehavior(launchWindow == .deletedFiles ? .presented : .automatic)

        WindowGroup("Version History", id: WindowID.versionHistory, for: URL.self) { $file in
            if let file = file ?? launchFile {
                VersionHistoryView(app: model, file: file)
                    .environment(model)
                    .buttonStyle(.secondary)
            }
        }
        .defaultSize(width: 720, height: 480)
        .defaultLaunchBehavior(launchFile == nil ? .automatic : .presented)

        Window("Shared Links", id: WindowID.sharedLinks) {
            SharedLinksView(app: model)
                .environment(model)
                .buttonStyle(.secondary)
        }
        .defaultSize(width: 760, height: 516)
        .defaultLaunchBehavior(AppModel.launchesSharedLinks(in: .standard) ? .presented : .automatic)

        WindowGroup("Share Link", id: WindowID.shareLink, for: URL.self) { $item in
            if let item = item ?? launchShareItem {
                ShareLinkView(app: model, item: item)
                    .environment(model)
                    .buttonStyle(.secondary)
            }
        }
        .windowResizability(.contentSize)
        .defaultLaunchBehavior(launchShareItem == nil ? .automatic : .presented)

        Settings {
            SettingsView()
                .environment(model)
                .buttonStyle(.secondary)
        }
    }

    private var launchFile: URL? {
        guard case .versionHistory(let path) = launchWindow else { return nil }
        return model.url(ofDisplayPath: path)
    }

    private var launchShareItem: URL? {
        guard case .shareLink(let path) = launchWindow else { return nil }
        return model.url(ofDisplayPath: path)
    }
}

enum WindowID {
    static let setup = "setup"
    static let questions = "questions"
    static let deletedFiles = "deleted-files"
    static let versionHistory = "version-history"
    static let shareLink = "share-link"
    static let sharedLinks = "shared-links"
}

extension OpenWindowAction {
    /// Brings the app forward, which a menu bar app must do itself.
    @MainActor
    func show(_ id: String) {
        self(id: id)
        NSApp.activate()
    }

    @MainActor
    func showVersionHistory(of file: URL) {
        self(id: WindowID.versionHistory, value: file)
        NSApp.activate()
    }

    @MainActor
    func showShareLink(for item: URL) {
        self(id: WindowID.shareLink, value: item)
        NSApp.activate()
    }
}
