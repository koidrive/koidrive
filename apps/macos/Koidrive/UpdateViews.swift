// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

extension EnvironmentValues {
    @Entry var shownUpdateNotice: UpdateNotice?
}

struct UpdatePopoverNotice: View {
    @Environment(AppModel.self) private var model
    @Environment(\.shownUpdateNotice) private var shown

    var body: some View {
        if let notice = shown ?? model.popoverUpdateNotice {
            VStack(alignment: .leading, spacing: 8) {
                HStack(alignment: .top, spacing: 8) {
                    UpdateIcon(notice: notice)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(verbatim: notice.title(popover: true))
                            .font(.body.weight(.semibold))
                            .foregroundStyle(Color.text)
                        if let detail = notice.detail(running: model.version, popover: true) {
                            Text(verbatim: detail)
                                .font(.callout)
                                .foregroundStyle(Color.text2)
                        }
                    }
                    .fixedSize(horizontal: false, vertical: true)
                    Spacer(minLength: 0)
                    if let dismiss = dismissAction(notice) {
                        Button(action: dismiss) {
                            Image(systemName: "xmark")
                                .font(.system(size: 9, weight: .semibold))
                                .foregroundStyle(Color.text2)
                                .frame(width: 12, height: 12)
                                .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .help("Dismiss")
                    }
                }
                if case .failed(let failure) = notice, let command = failure.fallbackCommand {
                    FallbackCommand(command: command)
                        .padding(.leading, 24)
                }
                UpdateActions(notice: notice)
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .padding(.horizontal, 7)
            .padding(.vertical, 10)
            .background(RoundedRectangle(cornerRadius: 8).fill(Color.fillStrong))
        }
    }

    private func dismissAction(_ notice: UpdateNotice) -> (() -> Void)? {
        switch notice {
        case .updated: model.acknowledgeUpdate
        case _ where notice.offersCheck: model.dismissUpdateCheck
        default: nil
        }
    }
}

struct UpdateSettingsRow: View {
    @Environment(AppModel.self) private var model
    @Environment(\.shownUpdateNotice) private var shown

    var body: some View {
        let notice = shown ?? model.updateNotice
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 8) {
                UpdateIcon(notice: notice)
                VStack(alignment: .leading, spacing: 2) {
                    Text(verbatim: notice.title(popover: false))
                        .foregroundStyle(Color.text)
                    if let detail = notice.detail(running: model.version, popover: false) {
                        Text(verbatim: detail)
                            .font(.callout)
                            .foregroundStyle(Color.text2)
                    }
                }
                .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 8)
                UpdateActions(notice: notice, offersCheck: true)
            }
            if case .failed(let failure) = notice, let command = failure.fallbackCommand {
                FallbackCommand(command: command)
                    .padding(.leading, 24)
            }
        }
    }
}

private struct UpdateIcon: View {
    let notice: UpdateNotice

    var body: some View {
        Group {
            switch notice {
            case .checking, .updating:
                ProgressView().controlSize(.mini)
            case .unchecked:
                Image(systemName: "questionmark.circle").foregroundStyle(Color.text2)
            case .checkFailed:
                Image(systemName: "exclamationmark.triangle").foregroundStyle(Color.statusYellow)
            case .available:
                Image(systemName: "arrow.down.circle").foregroundStyle(Color.text2)
            case .updated, .upToDate:
                Image(systemName: "checkmark").foregroundStyle(Color.statusGreen)
            case .failed:
                Image(systemName: "exclamationmark.triangle").foregroundStyle(Color.statusRed)
            case .offline:
                Image(systemName: "wifi.slash").foregroundStyle(Color.statusYellow)
            case .off:
                Image(systemName: "clock").foregroundStyle(Color.text2)
            }
        }
        .font(.system(size: 12))
        .frame(width: 16, height: 16)
    }
}

private struct UpdateActions: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openURL) private var openURL
    let notice: UpdateNotice
    var offersCheck = false

    var body: some View {
        HStack(spacing: 6) {
            switch notice {
            case .available(let version, let releaseURL):
                Button("What’s New") { openURL(releaseURL ?? notesURL(version)) }
                Button("Update", action: install)
                    .buttonStyle(.primary)
            case .updated(let version):
                Button("What’s New") { openURL(notesURL(version)) }
            case .failed(let failure):
                if let logPath = failure.logPath {
                    Button("Show Log") { NSWorkspace.shared.open(URL(filePath: logPath)) }
                }
                if failure.fallbackCommand == nil {
                    Button("Try Again", action: install)
                }
            case _ where offersCheck && notice.offersCheck:
                Button("Check for Updates") {
                    Task { await model.checkForUpdates() }
                }
            default:
                EmptyView()
            }
        }
        .controlSize(.small)
    }

    private func install() {
        Task {
            guard await model.installUpdate() != nil else { return }
            try? model.relaunch()
        }
    }

    private func notesURL(_ version: String) -> URL {
        URL(string: "https://github.com/koidrive/koidrive/releases/tag/v\(version)")!
    }
}

private struct FallbackCommand: View {
    let command: String

    var body: some View {
        HStack(spacing: 6) {
            Text(verbatim: command)
                .font(.callout.monospaced())
                .foregroundStyle(Color.text)
                .textSelection(.enabled)
                .lineLimit(1)
                .padding(.horizontal, 8)
                .frame(maxWidth: .infinity, minHeight: 22, alignment: .leading)
                .background(RoundedRectangle(cornerRadius: 5).fill(Color.group))
            Button("Copy") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(command, forType: .string)
            }
            .controlSize(.small)
        }
    }
}

private extension UpdateNotice {
    func title(popover: Bool) -> String {
        switch self {
        case .checking: String(localized: "Checking for updates…")
        case .unchecked:
            popover ? String(localized: "Koidrive hasn’t checked for updates") : String(localized: "Koidrive hasn’t checked for updates.")
        case .checkFailed:
            popover ? String(localized: "Couldn’t check for updates") : String(localized: "Couldn’t check for updates.")
        case .offline: String(localized: "Can’t check for updates while offline.")
        case .off: String(localized: "Update checks are off.")
        case .upToDate: popover ? String(localized: "Koidrive is up to date") : String(localized: "Koidrive is up to date.")
        case .available(let version, _):
            popover ? String(localized: "Koidrive \(version) is available") : String(localized: "Koidrive \(version) is available.")
        case .updating(let version): String(localized: "Updating to \(version)…")
        case .updated(let version): String(localized: "Updated to \(version)")
        case .failed(let failure) where failure.fallbackCommand != nil:
            popover ? String(localized: "Couldn’t update") : String(localized: "Couldn’t update.")
        case .failed(let failure):
            popover ? String(localized: "Couldn’t update to \(failure.version)") : String(localized: "Couldn’t update to \(failure.version).")
        }
    }

    func detail(running: String, popover: Bool) -> String? {
        switch self {
        case .checking, .unchecked: nil
        case .checkFailed(.unreachable): String(localized: "Check your internet connection and try again.")
        case .checkFailed(.refused): String(localized: "The update server didn’t answer. Try again later.")
        case .offline: String(localized: "Koidrive checks again when it reconnects.")
        case .off: String(localized: "They were turned off in Koidrive’s settings file.")
        case .upToDate(let checkedAtMs): checked(Date(timeIntervalSince1970: TimeInterval(checkedAtMs) / 1000))
        case .available: popover ? String(localized: "You have \(running).") : nil
        case .updating: String(localized: "Koidrive quits and reopens when it’s ready.")
        case .updated: String(localized: "Koidrive reopened after the update.")
        case .failed(let failure) where failure.fallbackCommand != nil:
            String(localized: "Koidrive \(running) still works. You can also update with Homebrew in Terminal:")
        case .failed(let failure):
            popover || failure.logPath == nil
                ? String(localized: "Koidrive \(running) still works.")
                : String(localized: "Koidrive \(running) still works. The log has the details.")
        }
    }

    private func checked(_ date: Date) -> String {
        let time = date.formatted(.dateTime.hour().minute())
        switch HistoryClock().day(of: date) {
        case .today: return String(localized: "Checked today at \(time).")
        case .yesterday: return String(localized: "Checked yesterday at \(time).")
        case .earlier: return String(localized: "Checked \(date.formatted(.dateTime.day().month(.abbreviated))) at \(time).")
        }
    }
}
