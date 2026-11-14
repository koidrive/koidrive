// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import ServiceManagement
import SwiftUI

enum SettingsTab: String {
    case folders, account, storage, problems, general

    static let storageKey = "settingsTab"
}

struct SettingsView: View {
    @Environment(AppModel.self) private var model
    @AppStorage(SettingsTab.storageKey) private var tab = SettingsTab.folders

    var body: some View {
        TabView(selection: $tab) {
            FoldersSettings()
                .tabItem { Label("Folders", systemImage: "folder") }
                .tag(SettingsTab.folders)
            AccountSettings()
                .tabItem { Label("Account", systemImage: "person.crop.circle") }
                .tag(SettingsTab.account)
            StorageSettings()
                .tabItem { Label("Storage", systemImage: "internaldrive") }
                .tag(SettingsTab.storage)
            ProblemsSettings()
                .tabItem { Label("Problems", systemImage: "exclamationmark.triangle") }
                .badge(model.state.problems.count)
                .tag(SettingsTab.problems)
            GeneralSettings()
                .tabItem { Label("General", systemImage: "gearshape") }
                .tag(SettingsTab.general)
        }
        .frame(width: 600, height: 440)
    }
}

struct FoldersSettings: View {
    @Environment(AppModel.self) private var model
    @State private var adding: SetupModel?
    @State private var unlinking: LinkedFolder?
    @State private var error: SessionError?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if model.state.folders.isEmpty {
                EmptyState(
                    systemImage: "folder",
                    title: "No folders",
                    message: Text(model.state.account == nil ? "Sign in first in the Account tab." : "Add a folder to start syncing.")
                ) {
                    addButton
                }
            } else {
                ScrollView {
                    VStack(spacing: 4) {
                        ForEach(model.state.folders, id: \.spaceId) { folder in
                            FolderRow(folder: folder) { unlinking = folder }
                        }
                    }
                }
                .frame(maxHeight: CGFloat(model.state.folders.count) * 48 - 4)
                .layoutPriority(1)
                if let error {
                    ErrorLine(error)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                }
                HStack {
                    Text("Linked folders can't be inside one another.")
                        .font(.callout)
                        .foregroundStyle(Color.text2)
                    Spacer()
                    addButton
                }
                .padding(.horizontal, 12)
                .padding(.top, 12)
                Spacer(minLength: 0)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color.panel)
        .sheet(item: $adding) { setup in
            AddFolderSheet(setup: setup) { adding = nil }
        }
        .alert(
            "Unlink \(unlinking?.spaceName ?? "")?",
            isPresented: Binding(get: { unlinking != nil }, set: { if !$0 { unlinking = nil } }),
            presenting: unlinking
        ) { folder in
            Button("Unlink", role: .destructive) {
                Task {
                    do {
                        try await model.unlinkFolder(spaceID: folder.spaceId)
                        error = nil
                    } catch let failure as SessionError {
                        error = failure
                    } catch {}
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("Files stay in the folder and on the server.")
        }
    }

    private var addButton: some View {
        Button("Add Folder…") {
            adding = SetupModel(app: model, mode: .addFolder, deviceName: Host.current().localizedName ?? "Mac")
        }
        .disabled(model.state.account == nil)
    }
}

private struct FolderRow: View {
    let folder: LinkedFolder
    let unlink: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "folder")
                .foregroundStyle(Color.label2)
                .frame(width: 20)
            Text(folder.spaceName)
                .fontWeight(.medium)
                .foregroundStyle(Color.text)
                .lineLimit(1)
                .frame(width: 76, alignment: .leading)
            Text(verbatim: abbreviatingHome(folder.path))
                .foregroundStyle(Color.text2)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(maxWidth: .infinity, alignment: .leading)
            StatusPill(title: folder.state.title, mark: folder.state.mark)
                .frame(width: 104, alignment: .leading)
            HStack(spacing: 6) {
                Button("Show in Finder") {
                    NSWorkspace.shared.open(URL(filePath: folder.path, directoryHint: .isDirectory))
                }
                Button("Unlink…", action: unlink)
            }
            .controlSize(.small)
            .fixedSize()
        }
        .padding(.horizontal, 12)
        .frame(height: 44)
    }
}

extension FolderState {
    var color: Color {
        switch self {
        case .upToDate: .statusGreen
        case .syncing: .koi
        case .recovering, .waiting, .offline: .statusYellow
        case .halted: .statusRed
        case .readOnly: .text3
        }
    }

    var mark: StatusPill.Mark {
        self == .syncing ? .progress : .dot(color)
    }
}

extension SetupModel: @retroactive Identifiable {}

private struct AddFolderSheet: View {
    @Bindable var setup: SetupModel
    let close: () -> Void

    var body: some View {
        FolderStep(setup: setup, cancel: close)
            .padding(24)
            .frame(width: 480)
            .background(Color.panel)
            .disabled(setup.isBusy)
            .onChange(of: setup.step) { _, step in
                if step == .done { close() }
            }
    }
}

struct AccountSettings: View {
    @Environment(AppModel.self) private var model
    @State private var devices: [Device] = []
    @State private var revoking: Device?
    @State private var pairing: PairingCode?
    @State private var confirmingSignOut = false
    @State private var confirmingLastDevice = false
    @State private var confirmingOffline = false
    @State private var error: SessionError?

    init(pairing: PairingCode? = nil) {
        _pairing = State(initialValue: pairing)
    }

    var body: some View {
        Group {
            if let account = model.state.account {
                ScrollView {
                    VStack(alignment: .leading, spacing: 4) {
                        GroupBand("Account")
                        PropertyRow("Server") {
                            Text(verbatim: account.serverUrl)
                                .font(.callout.monospaced())
                                .textSelection(.enabled)
                                .truncationMode(.middle)
                        }
                        if !account.userName.isEmpty {
                            PropertyRow("Owner") {
                                Text(verbatim: account.userName)
                            }
                        }
                        GroupBand("Devices")
                        ForEach(devices, id: \.deviceId) { device in
                            DeviceRow(device: device) { revoking = device }
                        }
                        HStack(spacing: 10) {
                            if let pairing {
                                VStack(alignment: .leading, spacing: 4) {
                                    PairingCodeLabel(code: pairing.code)
                                    Text("Enter it on the new device. Expires \(Date(milliseconds: pairing.expiresAtMs), format: .dateTime.hour().minute()).")
                                        .font(.callout)
                                        .foregroundStyle(Color.text2)
                                        .fixedSize(horizontal: false, vertical: true)
                                }
                            }
                            Spacer()
                            Button("Add a Device…") {
                                perform { pairing = try await model.startPairing() }
                            }
                            .controlSize(.small)
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                        if let error {
                            ErrorLine(error)
                                .padding(.horizontal, 12)
                                .padding(.vertical, 8)
                        }
                        HStack {
                            Spacer()
                            Button("Sign Out…", role: .destructive) { confirmingSignOut = true }
                                .controlSize(.small)
                        }
                        .padding(.horizontal, 12)
                        .padding(.top, 4)
                    }
                    .padding(12)
                }
                .task(id: account.deviceId) {
                    perform { devices = try await model.devices() }
                }
            } else {
                EmptyState(systemImage: "person", title: "Not signed in")
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color.panel)
        .alert(
            "Remove \(revoking?.name ?? "")?",
            isPresented: Binding(get: { revoking != nil }, set: { if !$0 { revoking = nil } }),
            presenting: revoking
        ) { device in
            Button("Remove", role: .destructive) {
                perform {
                    try await model.revokeDevice(device)
                    devices.removeAll { $0.deviceId == device.deviceId }
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("It stops syncing. Files already on it stay there.")
        }
        .alert("Sign out of Koidrive?", isPresented: $confirmingSignOut) {
            Button("Sign Out", role: .destructive) {
                perform {
                    do {
                        try await model.signOut()
                    } catch SessionError.LastDevice {
                        confirmingLastDevice = true
                    } catch SessionError.NotRevoked(_) {
                        confirmingOffline = true
                    }
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Every folder is unlinked. Files stay on this Mac and the server.")
        }
        .alert("Sign out of your last device?", isPresented: $confirmingLastDevice) {
            Button("Sign Out", role: .destructive) {
                perform { try await model.signOut(confirmLastDevice: true) }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("No other device is signed in. To get back in, run koi server add-device.")
        }
        .alert("Sign out without the server?", isPresented: $confirmingOffline) {
            Button("Sign Out", role: .destructive) {
                perform { try await model.signOut(confirmLastDevice: true) }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This Mac stays listed on the server until another device removes it.")
        }
    }

    private func perform(_ body: @escaping @MainActor () async throws -> Void) {
        Task {
            do {
                try await body()
                error = nil
            } catch let failure as SessionError {
                error = failure
            } catch {}
        }
    }
}

private struct PropertyRow<Value: View>: View {
    let title: LocalizedStringKey
    @ViewBuilder let value: Value

    init(_ title: LocalizedStringKey, @ViewBuilder value: () -> Value) {
        self.title = title
        self.value = value()
    }

    var body: some View {
        HStack(spacing: 10) {
            Text(title)
                .foregroundStyle(Color.text2)
            Spacer()
            value
                .foregroundStyle(Color.text)
                .lineLimit(1)
        }
        .padding(.horizontal, 12)
        .frame(height: 36)
    }
}

private struct DeviceRow: View {
    let device: Device
    let remove: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "laptopcomputer")
                .foregroundStyle(Color.label2)
                .frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 4) {
                    Text(verbatim: device.name)
                        .foregroundStyle(Color.text)
                    if device.thisDevice {
                        Text("(this Mac)").foregroundStyle(Color.text2)
                    }
                }
                Text("\(device.platformName), added \(Date(milliseconds: device.addedAtMs), format: .dateTime.day().month(.abbreviated).year())")
                    .font(.callout)
                    .foregroundStyle(Color.text2)
            }
            Spacer()
            if !device.thisDevice {
                Button("Remove…", role: .destructive, action: remove)
                    .controlSize(.small)
            }
        }
        .padding(.horizontal, 12)
        .frame(height: 48)
    }
}

extension Device {
    var platformName: String {
        switch platform {
        case "macos": "macOS"
        case "linux": "Linux"
        case "windows": "Windows"
        default: platform
        }
    }
}

private struct PairingCodeLabel: View {
    let code: String
    @State private var copied = false

    var body: some View {
        HStack(spacing: 8) {
            Text("Pairing code")
                .font(.callout)
                .foregroundStyle(Color.text2)
            Text(verbatim: code)
                .font(.body.monospaced().weight(.semibold))
                .foregroundStyle(Color.text)
                .textSelection(.enabled)
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(code, forType: .string)
                copied = true
            } label: {
                Image(systemName: copied ? "checkmark" : "doc.on.doc")
            }
            .buttonStyle(.plain)
            .foregroundStyle(Color.text2)
            .help("Copy")
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(RoundedRectangle(cornerRadius: 6).fill(Color.group))
        .fixedSize()
        .onChange(of: code) { copied = false }
    }
}

struct StorageSettings: View {
    @Environment(AppModel.self) private var model
    @State private var storage = StorageModel()

    var body: some View {
        Group {
            if let account = model.state.account {
                ScrollView {
                    VStack(alignment: .leading, spacing: 4) {
                        let spaces = storage.spaces ?? []
                        if spaces.count == 1 {
                            usage(of: spaces[0])
                        } else {
                            ForEach(spaces) { entry in
                                GroupBand(verbatim: entry.space.name)
                                usage(of: entry)
                            }
                        }
                        if storage.spaces?.isEmpty == true {
                            Text("This account has no spaces yet.")
                                .foregroundStyle(Color.text2)
                                .padding(.horizontal, 12)
                                .padding(.vertical, 10)
                        }
                        if let error = storage.error {
                            ErrorLine(error)
                                .padding(.horizontal, 12)
                                .padding(.vertical, 4)
                        }
                        HStack(spacing: 12) {
                            Text("Each chunk counts once. Unreferenced: uploads in progress and chunks awaiting deletion.")
                                .font(.callout)
                                .foregroundStyle(Color.text2)
                                .fixedSize(horizontal: false, vertical: true)
                            Spacer()
                            if storage.isLoading {
                                ProgressView().controlSize(.small)
                            } else {
                                Button("Refresh") { Task { await storage.load(model.storage) } }
                                    .controlSize(.small)
                            }
                        }
                        .padding(.horizontal, 12)
                        .padding(.top, 10)
                    }
                    .padding(12)
                }
                .task(id: account.deviceId) { await storage.load(model.storage) }
            } else {
                EmptyState(systemImage: "person", title: "Not signed in")
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color.panel)
    }

    @ViewBuilder
    private func usage(of entry: SpaceStorage) -> some View {
        switch entry.usage {
        case .success(let usage):
            StorageRow(title: "Live files", count: "^[\(usage.liveFiles) file](inflect: true)", bytes: usage.liveBytes)
            StorageRow(title: "Deleted files", count: "^[\(usage.trashedFiles) file](inflect: true)", bytes: usage.trashBytes)
            StorageRow(title: "Old versions", count: "^[\(usage.oldVersions) version](inflect: true)", bytes: usage.oldVersionBytes)
            StorageRow(title: "Unreferenced", count: nil, bytes: usage.unreferencedBytes)
            StorageRow(title: "Total", count: nil, bytes: usage.totalBytes, isTotal: true)
        case .failure(let refusal):
            ErrorLine(message: refusal.message)
                .padding(.horizontal, 12)
                .padding(.vertical, 4)
        }
    }
}

private struct StorageRow: View {
    let title: LocalizedStringKey
    let count: LocalizedStringKey?
    let bytes: UInt64
    var isTotal = false

    var body: some View {
        HStack(spacing: 10) {
            Text(title)
                .foregroundStyle(isTotal ? Color.text : Color.text2)
            Spacer()
            if let count {
                Text(count).foregroundStyle(Color.text2)
            }
            Text(Int64(clamping: bytes), format: .byteCount(style: .file))
                .monospacedDigit()
                .foregroundStyle(Color.text)
                .frame(minWidth: 70, alignment: .trailing)
        }
        .fontWeight(isTotal ? .semibold : nil)
        .padding(.horizontal, 12)
        .frame(height: 36)
    }
}

struct ProblemsSettings: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            if model.state.problems.isEmpty {
                EmptyState(systemImage: "checkmark.circle", title: "No problems")
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(model.problemSections) { section in
                            ProblemBand(spaceID: section.spaceID, folder: section.folder)
                            ForEach(section.problems) { problem in
                                ProblemRow(problem: problem, folder: section.folder)
                            }
                        }
                    }
                    .padding(12)
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color.panel)
    }
}

private struct ProblemBand: View {
    let spaceID: String?
    let folder: LinkedFolder?

    var body: some View {
        if let folder {
            GroupBand(verbatim: folder.spaceName)
        } else {
            GroupBand(title: "Unlinked space") {
                if let spaceID {
                    Text(verbatim: spaceID)
                        .foregroundStyle(Color.text2)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }
        }
    }
}

private struct ProblemRow: View {
    let problem: Problem
    let folder: LinkedFolder?

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "exclamationmark.triangle")
                .foregroundStyle(Color.statusRed)
                .padding(.top, 2)
            VStack(alignment: .leading, spacing: 3) {
                Text(problem.kind.title)
                    .fontWeight(.semibold)
                    .foregroundStyle(Color.text)
                if let path = problem.path {
                    Text(verbatim: path)
                        .font(.system(size: 12))
                        .foregroundStyle(Color.text2)
                        .textSelection(.enabled)
                } else if let folder, problem.kind.concernsTheFolder {
                    Text(verbatim: abbreviatingHome(folder.path))
                        .font(.system(size: 12))
                        .foregroundStyle(Color.text2)
                        .textSelection(.enabled)
                }
                Text(problem.kind.advice)
                    .foregroundStyle(Color.text2)
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
                Text("Since \(Date(milliseconds: problem.sinceMs), format: .dateTime.day().month(.abbreviated).hour().minute())")
                    .font(.callout)
                    .foregroundStyle(Color.text3)
            }
            Spacer()
            if let folder {
                Button("Show in Finder") {
                    let url = URL(filePath: folder.path)
                    if let path = problem.path {
                        NSWorkspace.shared.activateFileViewerSelecting([url.appending(path: path)])
                    } else if FileManager.default.fileExists(atPath: folder.path) {
                        NSWorkspace.shared.open(url)
                    } else {
                        var parent = url.deletingLastPathComponent()
                        while !FileManager.default.fileExists(atPath: parent.path(percentEncoded: false)) {
                            parent.deleteLastPathComponent()
                        }
                        NSWorkspace.shared.open(parent)
                    }
                }
                .controlSize(.small)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }
}

extension ProblemKind {
    var concernsTheFolder: Bool {
        switch self {
        case .folderMissing, .folderNotAFolder, .folderUnreadable, .folderOnOtherVolume, .markerMissing,
             .markerUnreadable, .markerOfOtherSpace, .diskOlderThanSyncState, .unsupportedFileSystem:
            true
        default:
            false
        }
    }
}

struct GeneralSettings: View {
    @Environment(AppModel.self) private var app
    @State private var loginItemStatus = SMAppService.mainApp.status
    @State private var openAtLogin = false
    @State private var loginItemError: String?
    @State private var telemetryError: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Open Koidrive at login")
                        .foregroundStyle(Color.text)
                    if loginItemStatus == .requiresApproval {
                        Text("Allow Koidrive in Login Items.")
                            .font(.callout)
                            .foregroundStyle(Color.text2)
                    }
                    if let loginItemError {
                        Text(loginItemError)
                            .font(.callout)
                            .foregroundStyle(Color.statusRed)
                    }
                }
                Spacer()
                if loginItemStatus == .requiresApproval {
                    Button("Open Login Items") { SMAppService.openSystemSettingsLoginItems() }
                        .controlSize(.small)
                }
                Toggle("Open Koidrive at login", isOn: $openAtLogin)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .controlSize(.small)
                    .tint(.koi)
                    .onChange(of: openAtLogin) { _, on in
                        if on != isRegistered {
                            setOpenAtLogin(on)
                        }
                    }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            Rectangle()
                .fill(Color.line)
                .frame(height: 1)
                .padding(.horizontal, 12)
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Send anonymous usage reports")
                        .foregroundStyle(Color.text)
                    Text("It never shares file names, paths or contents.")
                        .font(.callout)
                        .foregroundStyle(Color.text2)
                    if app.usageReportsTurnedOffByVariable {
                        Text("Turned off by an environment variable.")
                            .font(.callout)
                            .foregroundStyle(Color.text2)
                    }
                    if let telemetryError {
                        Text(telemetryError)
                            .font(.callout)
                            .foregroundStyle(Color.statusRed)
                    }
                }
                Spacer()
                Toggle("Send anonymous usage reports", isOn: Binding(get: { app.telemetry.enabled }, set: setTelemetry))
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .controlSize(.small)
                    .tint(.koi)
                    .disabled(app.telemetry.locked)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            GroupBand("About")
            VStack(alignment: .leading, spacing: 2) {
                Text(verbatim: "Koidrive " + (Bundle.main.object(forInfoDictionaryKey: "KoidriveVersion") as? String ?? KoidriveFFI.version()))
                    .foregroundStyle(Color.text)
                Text("Command-line tool: koi")
                    .font(.callout.monospaced())
                    .foregroundStyle(Color.text2)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
            .help(Text(verbatim: "Core \(KoidriveFFI.version())"))
            Spacer()
        }
        .padding(12)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color.panel)
        .onAppear(perform: readStatus)
    }
    private var isRegistered: Bool {
        loginItemStatus == .enabled || loginItemStatus == .requiresApproval
    }

    private func readStatus() {
        loginItemStatus = SMAppService.mainApp.status
        openAtLogin = isRegistered
    }

    private func setOpenAtLogin(_ on: Bool) {
        do {
            if on {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            loginItemError = nil
        } catch {
            loginItemError = error.localizedDescription
        }
        readStatus()
    }

    private func setTelemetry(_ on: Bool) {
        do {
            try app.setTelemetry(enabled: on)
            telemetryError = nil
        } catch {
            telemetryError = (error as? SessionError)?.message ?? error.localizedDescription
        }
    }
}

#Preview("Folders") {
    SettingsView().environment(AppModel.preview(.upToDate))
}

#Preview("Problems") {
    ProblemsSettings().environment(AppModel.preview(.problems)).frame(width: 600, height: 440)
}

#Preview("Storage: one space") {
    StorageSettings().environment(AppModel.previewWithOneSpace()).frame(width: 600, height: 440)
}

#Preview("Storage: two spaces") {
    StorageSettings().environment(AppModel.preview(.upToDate)).frame(width: 600, height: 440)
}

#Preview("Account") {
    AccountSettings().environment(AppModel.preview(.upToDate)).frame(width: 600, height: 440)
}

#Preview("General: usage reports on") {
    GeneralSettings().environment(AppModel.preview(.upToDate, telemetry: TelemetrySetting(enabled: true, decided: true, locked: false)))
        .frame(width: 600, height: 440)
}

#Preview("General: usage reports off") {
    GeneralSettings().environment(AppModel.preview(.upToDate, telemetry: TelemetrySetting(enabled: false, decided: true, locked: false)))
        .frame(width: 600, height: 440)
}

#Preview("General: usage reports undecided") {
    GeneralSettings().environment(AppModel.preview(.upToDate, telemetry: TelemetrySetting(enabled: false, decided: false, locked: false)))
        .frame(width: 600, height: 440)
}

#Preview("General: usage reports locked") {
    GeneralSettings().environment(AppModel.preview(.upToDate, telemetry: TelemetrySetting(enabled: false, decided: false, locked: true)))
        .frame(width: 600, height: 440)
}
