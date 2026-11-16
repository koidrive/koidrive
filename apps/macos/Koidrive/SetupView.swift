// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

struct SetupView: View {
    @Environment(AppModel.self) private var app
    @State private var setup: SetupModel?

    var body: some View {
        Group {
            if let setup {
                SetupSteps(setup: setup)
            } else {
                ProgressView()
                    .controlSize(.small)
                    .frame(maxWidth: .infinity, minHeight: 204)
                    .background(Color.panel)
            }
        }
        .frame(width: 480)
        .onAppear {
            if setup == nil {
                setup = SetupModel(app: app, mode: .firstRun, deviceName: Host.current().localizedName ?? "Mac")
            }
        }
    }
}

struct SetupSteps: View {
    @Bindable var setup: SetupModel
    @Environment(\.dismissWindow) private var dismissWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            if setup.step != .done {
                StepIndicator(step: setup.step)
            }
            switch setup.step {
            case .server: ServerStep(setup: setup)
            case .signIn: SignInStep(setup: setup)
            case .folder: FolderStep(setup: setup)
            case .done: DoneStep { dismissWindow(id: WindowID.setup) }
            }
        }
        .padding(.horizontal, 24)
        .padding(.top, 20)
        .padding(.bottom, 24)
        .background(Color.panel)
        .disabled(setup.isBusy)
    }
}

private struct StepIndicator: View {
    let step: SetupModel.Step

    var body: some View {
        HStack(spacing: 16) {
            item(1, "Server", for: .server)
            item(2, "Sign in", for: .signIn)
            item(3, "Folder", for: .folder)
        }
        .font(.callout)
    }

    private func item(_ number: Int, _ title: LocalizedStringKey, for target: SetupModel.Step) -> some View {
        let reached = target.order <= step.order
        return HStack(spacing: 6) {
            Text(verbatim: number.formatted())
                .font(.system(size: 10, weight: .semibold))
                .foregroundStyle(reached ? Color.onInk : Color.text2)
                .frame(width: 16, height: 16)
                .background(Circle().fill(reached ? Color.ink : Color.group))
            Text(title)
                .fontWeight(step == target ? .semibold : .regular)
                .foregroundStyle(step == target ? Color.text : Color.text2)
        }
    }
}

extension SetupModel.Step {
    fileprivate var order: Int {
        switch self {
        case .server: 0
        case .signIn: 1
        case .folder: 2
        case .done: 3
        }
    }
}

private struct StepHeader: View {
    let title: LocalizedStringKey
    let help: String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Color.text)
            Text(help)
                .foregroundStyle(Color.text2)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct Field<Accessory: View>: View {
    let label: LocalizedStringKey
    @Binding var text: String
    var monospaced = false
    var invalid = false
    var prompt: Text?
    @ViewBuilder var accessory: Accessory

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label)
                .foregroundStyle(Color.text2)
            TextField(label, text: $text, prompt: prompt)
                .labelsHidden()
                .textFieldStyle(.roundedBorder)
                .font(monospaced ? .body.monospaced() : .body)
                .overlay {
                    if invalid {
                        RoundedRectangle(cornerRadius: 6).strokeBorder(Color.statusRed)
                    }
                }
            accessory
        }
    }
}

extension Field where Accessory == EmptyView {
    init(label: LocalizedStringKey, text: Binding<String>, monospaced: Bool = false, invalid: Bool = false,
         prompt: Text? = nil) {
        self.init(label: label, text: text, monospaced: monospaced, invalid: invalid, prompt: prompt) { EmptyView() }
    }
}

private struct SetupErrorLine: View {
    let message: String

    var body: some View {
        ErrorLine(message: message)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color.redTint))
    }
}

private struct ActionRow<Leading: View, Primary: View>: View {
    @ViewBuilder var leading: Leading
    @ViewBuilder var primary: Primary

    var body: some View {
        HStack {
            leading
            Spacer()
            primary
                .buttonStyle(.primary)
                .keyboardShortcut(.defaultAction)
        }
        .padding(.top, 4)
    }
}

extension ActionRow where Leading == EmptyView {
    init(@ViewBuilder primary: () -> Primary) {
        self.init(leading: { EmptyView() }, primary: primary)
    }
}

private struct ServerStep: View {
    @Bindable var setup: SetupModel

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            StepHeader(
                title: "Connect to your server",
                help: String(localized: "For example, https://koidrive.alice.workers.dev.")
            )
            Field(label: "Server address", text: $setup.serverURL, invalid: setup.error != nil,
                  prompt: Text(verbatim: "https://"))
                .onSubmit(connect)
            if setup.asksForUsageReports {
                Toggle(isOn: $setup.sendsUsageReports) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Send usage reports")
                            .foregroundStyle(Color.text)
                        Text("It never shares file names, paths or contents.")
                            .font(.callout)
                            .foregroundStyle(Color.text2)
                    }
                }
                .toggleStyle(.checkbox)
                .tint(.koi)
            }
            if let error = setup.error {
                SetupErrorLine(message: Self.message(for: error))
                    .help(error.detail ?? "")
            }
            ActionRow {
                Button("Connect", action: connect)
                    .disabled(!setup.canConnect)
            }
        }
    }

    private static func message(for error: SessionError) -> String {
        guard case .ServerUnreachable = error else { return error.message }
        return String(localized: "\(error.message) Check the address.")
    }

    private func connect() {
        Task { await setup.connect() }
    }
}

private struct SignInStep: View {
    @Bindable var setup: SetupModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if setup.claimsServer {
                StepHeader(
                    title: "Claim this server",
                    help: String(localized: "Find \"Koidrive claim code\" in the Worker's logs in Cloudflare.")
                )
            } else {
                StepHeader(
                    title: "Sign in",
                    help: String(localized: "Get a code from Settings > Account on a signed-in device.")
                )
            }
            if let server = setup.server {
                HStack(spacing: 8) {
                    Image(systemName: "server.rack")
                        .foregroundStyle(Color.text2)
                    Text(verbatim: server.url)
                        .foregroundStyle(Color.text)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Spacer(minLength: 0)
                    Text(verbatim: "v" + server.serverVersion)
                        .font(.callout.weight(.medium))
                        .foregroundStyle(Color.text2)
                        .padding(.horizontal, 8)
                        .frame(height: 22)
                        .overlay(Capsule().strokeBorder(Color.chipBorder))
                }
                .padding(.horizontal, 12)
                .frame(height: 36)
                .background(RoundedRectangle(cornerRadius: 6).fill(Color.group))
            }
            Group {
                if setup.claimsServer {
                    Field(label: "Your name", text: $setup.userName)
                    Field(label: "Claim code", text: $setup.code, monospaced: true, invalid: codeIsWrong)
                } else {
                    Field(label: "Pairing code", text: $setup.code, monospaced: true, invalid: codeIsWrong) {
                        Text("Codes expire after a few minutes.")
                            .font(.callout)
                            .foregroundStyle(Color.text2)
                    }
                }
                Field(label: "Name of this Mac", text: $setup.deviceName)
            }
            .onSubmit(signIn)
            if let error = setup.error {
                SetupErrorLine(message: error.message)
                    .help(error.detail ?? "")
            }
            ActionRow {
                Button("Change Server") { setup.changeServer() }
            } primary: {
                Button(setup.claimsServer ? "Claim Server" : "Sign In", action: signIn)
                    .disabled(!setup.canSignIn)
            }
        }
    }

    private var codeIsWrong: Bool {
        switch setup.error {
        case .InvalidCode, .TooManyAttempts, .CodeForOtherServer: true
        default: false
        }
    }

    private func signIn() {
        Task { await setup.signIn() }
    }
}

struct FolderStep: View {
    @Bindable var setup: SetupModel
    var cancel: (() -> Void)?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            StepHeader(
                title: "Choose a folder to sync",
                help: String(localized: "The folder and the space sync both ways.")
            )
            VStack(alignment: .leading, spacing: 4) {
                Text("Space")
                    .foregroundStyle(Color.text2)
                Picker("Space", selection: $setup.selectedSpaceID) {
                    ForEach(setup.spaces, id: \.spaceId) { space in
                        Text(space.name).tag(Optional(space.spaceId))
                    }
                    if !setup.spaces.isEmpty {
                        Divider()
                    }
                    Text("New Space…").tag(String?.none)
                }
                .labelsHidden()
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            if setup.selectedSpaceID == nil {
                Field(label: "Space name", text: $setup.newSpaceName)
            }
            VStack(alignment: .leading, spacing: 4) {
                Text("Folder")
                    .foregroundStyle(Color.text2)
                HStack(spacing: 8) {
                    HStack(spacing: 6) {
                        Image(systemName: "folder")
                            .foregroundStyle(Color.text2)
                        if let folderPath = setup.folderPath {
                            Text(verbatim: abbreviatingHome(folderPath))
                                .font(.callout.monospaced())
                                .foregroundStyle(Color.text)
                                .lineLimit(1)
                                .truncationMode(.middle)
                        } else {
                            Text("None")
                                .foregroundStyle(Color.text2)
                        }
                    }
                    .padding(.horizontal, 8)
                    .frame(maxWidth: .infinity, minHeight: 24, alignment: .leading)
                    .background(RoundedRectangle(cornerRadius: 6).fill(Color.group))
                    Button("Choose…", action: chooseFolder)
                }
            }
            if let error = setup.error {
                SetupErrorLine(message: error.message)
                    .help(error.detail ?? "")
            }
            ActionRow {
                if let cancel {
                    Button("Cancel", action: cancel)
                        .keyboardShortcut(.cancelAction)
                }
            } primary: {
                Button("Link Folder") {
                    Task { await setup.link() }
                }
                .disabled(!setup.canLink)
            }
        }
        .task { await setup.loadSpaces() }
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = String(localized: "Choose")
        if panel.runModal() == .OK, let url = panel.url {
            setup.folderPath = url.path(percentEncoded: false)
        }
    }
}

func abbreviatingHome(_ path: String) -> String {
    (path as NSString).abbreviatingWithTildeInPath
}

private struct DoneStep: View {
    @Environment(AppModel.self) private var app
    let close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            VStack(spacing: 6) {
                Image(systemName: "checkmark.circle")
                    .font(.system(size: 40, weight: .light))
                    .foregroundStyle(Color.statusGreen)
                    .padding(.bottom, 8)
                Text("Koidrive is set up")
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Color.text)
                Text("Koidrive runs in the menu bar.")
                    .foregroundStyle(Color.text2)
                    .padding(.bottom, 8)
                ForEach(app.state.folders, id: \.spaceId) { folder in
                    HStack(spacing: 8) {
                        Image(systemName: "folder")
                            .foregroundStyle(Color.text2)
                        Text(folder.spaceName)
                            .fontWeight(.medium)
                            .foregroundStyle(Color.text)
                        Text(verbatim: abbreviatingHome(folder.path))
                            .font(.callout.monospaced())
                            .foregroundStyle(Color.text2)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        Spacer(minLength: 0)
                    }
                    .padding(.horizontal, 12)
                    .frame(height: 44)
                    .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color.line))
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.top, 12)
            ActionRow {
                Button("Done", action: close)
            }
        }
    }
}

#Preview("Server") {
    SetupView().environment(AppModel.preview(.firstRun))
}

#Preview("Server: usage reports undecided") {
    serverStep(TelemetrySetting(enabled: false, decided: false, locked: false))
}

#Preview("Server: usage reports on") {
    serverStep(TelemetrySetting(enabled: true, decided: true, locked: false))
}

#Preview("Server: usage reports off") {
    serverStep(TelemetrySetting(enabled: false, decided: true, locked: false))
}

#Preview("Server: usage reports locked") {
    serverStep(TelemetrySetting(enabled: false, decided: false, locked: true))
}

@MainActor
private func serverStep(_ telemetry: TelemetrySetting) -> some View {
    let app = AppModel.preview(.firstRun, telemetry: telemetry)
    return SetupSteps(setup: SetupModel(app: app, mode: .firstRun, deviceName: "Mac")).frame(width: 480).environment(app)
}

#Preview("Folder") {
    let app = AppModel.preview(.upToDate)
    let setup = SetupModel(app: app, mode: .addFolder, deviceName: "Mac")
    return FolderStep(setup: setup).padding(24).frame(width: 480).environment(app)
}
