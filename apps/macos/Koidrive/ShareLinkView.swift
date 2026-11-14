// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

struct ShareLinkView: View {
    @Environment(AppModel.self) private var app
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismiss) private var dismiss
    @State private var share: ShareLinkModel
    @State private var copied = false
    @FocusState private var passwordFocused: Bool

    init(app: AppModel, item: URL) {
        self.init(share: ShareLinkModel(app: app, item: item))
    }

    init(share: ShareLinkModel) {
        _share = State(initialValue: share)
    }

    var body: some View {
        VStack(spacing: 0) {
            bar
            VStack(alignment: .leading, spacing: 16) {
                access
                if let made = share.link {
                    created(made)
                } else {
                    options
                }
                status
            }
            .padding(.horizontal, 20)
            .padding(.top, 16)
            .padding(.bottom, 20)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            footer
        }
        .background(Color.panel)
        .frame(width: 560)
        .frame(minHeight: 352)
        .navigationTitle("Share Link")
        .onChange(of: app.state.status) { share.sessionStatusChanged() }
        .onChange(of: share.folder?.state) { share.sessionStatusChanged() }
        .onChange(of: share.requiresPassword) { _, required in passwordFocused = required }
    }

    private var bar: some View {
        HistoryBar {
            Image(systemName: share.isFolder ? "folder" : "doc.text")
                .font(.system(size: 15))
                .foregroundStyle(Color.text2)
                .frame(width: 20)
            Text(verbatim: share.name)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Color.text)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer(minLength: 0)
            if app.state.status == .offline {
                StatusPill(title: String(localized: "Offline"), mark: .dot(.text3))
            } else if let folder = share.folder {
                StatusPill(title: folder.state.title, mark: folder.state.mark)
            }
        }
    }

    private var access: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: "eye")
                .foregroundStyle(Color.text2)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 2) {
                Text(share.isFolder
                    ? "Anyone with the link can browse and download this folder, including files added later."
                    : "Anyone with the link can download this file.")
                    .foregroundStyle(Color.text)
                    .fixedSize(horizontal: false, vertical: true)
                Text("Read-only. They can’t change anything.")
                    .font(.callout)
                    .foregroundStyle(Color.text2)
            }
            .frame(maxWidth: 440, alignment: .leading)
        }
    }

    private var options: some View {
        RowGroup {
            OptionRow(label: "Expires") {
                Picker("Expires", selection: $share.expiry) {
                    ForEach([ShareExpiry.day, .week, .month], id: \.self) { expiry in
                        Text(verbatim: expiry.title).tag(expiry)
                    }
                }
                .pickerStyle(.menu)
                .labelsHidden()
                .fixedSize()
            }
            Divider().padding(.horizontal, 12)
            OptionRow(label: "Password") {
                Toggle("Require a password", isOn: $share.requiresPassword)
                    .toggleStyle(.checkbox)
            }
            if share.requiresPassword {
                Divider().padding(.horizontal, 12)
                VStack(alignment: .leading, spacing: 6) {
                    SecureField("Password", text: $share.password)
                        .textFieldStyle(.roundedBorder)
                        .labelsHidden()
                        .focused($passwordFocused)
                        .onSubmit { Task { await share.create() } }
                    switch share.passwordIssue {
                    case .tooShort: ErrorLine(message: String(localized: "Use at least 8 characters."))
                    case .tooLong: ErrorLine(message: String(localized: "Use at most 256 characters."))
                    case nil:
                        Text("At least 8 characters.")
                            .font(.callout)
                            .foregroundStyle(Color.text2)
                    }
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
            }
        }
        .disabled(!share.canCreate)
    }

    private func created(_ made: NewShareLink) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            RowGroup {
                HStack(spacing: 12) {
                    Image(systemName: "link")
                        .foregroundStyle(Color.text2)
                    Text(verbatim: made.url.split(separator: "://", maxSplits: 1).last.map(String.init) ?? made.url)
                        .foregroundStyle(Color.text)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .accessibilityIdentifier("share-url")
                    Button(copied ? "Copied" : "Copy") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(made.url, forType: .string)
                        copied = true
                    }
                    .controlSize(.small)
                }
                .optionRow()
                Divider().padding(.horizontal, 12)
                HStack(spacing: 12) {
                    Text("Ends")
                        .foregroundStyle(Color.text)
                        .frame(width: 70, alignment: .leading)
                    Text(verbatim: share.expiry.title)
                        .foregroundStyle(Color.text2)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    if made.link.hasPassword {
                        Label("Password", systemImage: "lock")
                            .foregroundStyle(Color.text2)
                    }
                }
                .optionRow()
            }
            Text("Copy it now. Koidrive won’t show this link again.")
                .foregroundStyle(Color.text2)
        }
    }

    @ViewBuilder
    private var status: some View {
        if share.isOffline {
            Label {
                Text("You can share once Koidrive reconnects.")
                    .foregroundStyle(Color.text2)
            } icon: {
                Image(systemName: "bolt.circle")
                    .foregroundStyle(Color.statusYellow)
            }
        } else {
            switch share.step {
            case .checking:
                HStack(spacing: 8) {
                    ProgressView()
                        .controlSize(.small)
                    Text("Checking files before sharing…")
                        .foregroundStyle(Color.text2)
                }
            case .notSyncedYet:
                HStack(spacing: 8) {
                    ProgressArc()
                        .frame(width: 8, height: 8)
                    Text("You can share it once it’s up to date.")
                        .foregroundStyle(Color.text2)
                }
            case .failed(let error):
                ErrorLine(message: error.shareFailure)
            case .choosing, .created:
                EmptyView()
            }
        }
    }

    private var footer: some View {
        HistoryFooter {
            Button("Shared Links…") { openWindow.show(WindowID.sharedLinks) }
            Spacer(minLength: 0)
            if share.link != nil {
                Button("Done") { dismiss() }
                    .buttonStyle(.primary)
                    .keyboardShortcut(.defaultAction)
            } else {
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(isRetry ? "Try Again" : "Create Link") { Task { await share.create() } }
                    .buttonStyle(.primary)
                    .disabled(!share.canCreate)
                    .keyboardShortcut(.defaultAction)
            }
        }
    }

    private var isRetry: Bool {
        if case .failed = share.step { !share.isOffline } else { false }
    }
}

private struct OptionRow<Control: View>: View {
    let label: LocalizedStringKey
    @ViewBuilder let control: Control

    var body: some View {
        HStack(spacing: 12) {
            Text(label)
                .foregroundStyle(Color.text)
                .frame(width: 70, alignment: .leading)
            Spacer(minLength: 0)
            control
        }
        .optionRow()
    }
}

private extension View {
    func optionRow() -> some View {
        padding(.horizontal, 12)
            .padding(.vertical, 6)
            .frame(minHeight: 36)
    }
}

#Preview("Share Link") {
    let app = AppModel.preview(.upToDate)
    let file = (app.state.folders.first?.url ?? URL(filePath: "/")).appending(path: "Budget 2026.numbers")
    return ShareLinkView(app: app, item: file).environment(app)
}
