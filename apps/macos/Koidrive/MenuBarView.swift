// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

struct MenuBarLabel: View {
    let model: AppModel
    @Environment(\.openWindow) private var openWindow
    @State private var reduceMotion = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
    @State private var turningArrows = TurningArrows()

    var body: some View {
        Image(turningArrows.isTurning ? "MenuBar/syncing-fish" : model.iconBadge.imageName)
            .accessibilityLabel(Text("Koidrive: \(model.headline == .questions ? String(localized: "Needs you") : model.headlineTitle)"))
            .onChange(of: model.turnsMenuBarArrows(reduceMotion: reduceMotion), initial: true) { _, turns in
                turningArrows.turn(turns)
            }
            .onReceive(NSWorkspace.shared.notificationCenter.publisher(for: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification)) { _ in
                reduceMotion = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
            }
            .task {
                if model.needsSetup {
                    openWindow.show(WindowID.setup)
                }
            }
            .onChange(of: model.hasUnseenQuestions, initial: true) { _, unseen in
                if unseen {
                    openWindow.show(WindowID.questions)
                }
            }
    }
}

@MainActor
@Observable
final class TurningArrows {
    private(set) var isTurning = false
    @ObservationIgnored private var arrows: NSImageView?

    func turn(_ on: Bool) {
        arrows?.removeFromSuperview()
        arrows = nil
        if on, let button = NSApp.windows.lazy.compactMap({ $0.contentView?.statusBarButton }).first {
            arrows = Self.addArrows(to: button)
        }
        isTurning = arrows != nil
    }

    private static func addArrows(to button: NSStatusBarButton) -> NSImageView? {
        guard let image = NSImage(named: "MenuBar/syncing-arrows") else { return nil }
        let arrows = NSImageView(image: image)
        arrows.imageScaling = .scaleNone
        arrows.wantsLayer = true
        arrows.translatesAutoresizingMaskIntoConstraints = false
        button.addSubview(arrows)
        NSLayoutConstraint.activate([
            arrows.widthAnchor.constraint(equalToConstant: image.size.width),
            arrows.heightAnchor.constraint(equalToConstant: image.size.height),
            arrows.centerXAnchor.constraint(equalTo: button.centerXAnchor),
            arrows.centerYAnchor.constraint(equalTo: button.centerYAnchor),
        ])
        guard let layer = arrows.layer else {
            arrows.removeFromSuperview()
            return nil
        }
        let center = CGPoint(x: 14.15, y: 14.15)
        let steps = 72
        let turn = CAKeyframeAnimation(keyPath: "transform")
        turn.values = (0...steps).map { step in
            let turned = CATransform3DRotate(CATransform3DMakeTranslation(center.x, center.y, 0), -2 * .pi * Double(step) / Double(steps), 0, 0, 1)
            return CATransform3DTranslate(turned, -center.x, -center.y, 0)
        }
        turn.duration = 1.6
        turn.repeatCount = .infinity
        turn.preferredFrameRateRange = CAFrameRateRange(minimum: 15, maximum: 20, preferred: 20)
        layer.add(turn, forKey: "turn")
        return arrows
    }
}

private extension NSView {
    var statusBarButton: NSStatusBarButton? {
        self as? NSStatusBarButton ?? subviews.lazy.compactMap(\.statusBarButton).first
    }
}

extension IconBadge {
    var imageName: String {
        switch self {
        case .none: "MenuBar/up-to-date"
        case .syncing: "MenuBar/syncing"
        case .paused: "MenuBar/paused"
        case .offline: "MenuBar/offline"
        case .question: "MenuBar/question"
        case .problem: "MenuBar/problem"
        }
    }
}

extension AppModel {
    var headlineTitle: String {
        switch headline {
        case .setup: SyncStatus.needsSetup.title
        case .halted: SyncStatus.halted.title
        case .questions: String(localized: "Needs your answer")
        case .status(let status): status.title
        }
    }

    var headlineSubtitle: String? {
        switch headline {
        case .setup:
            return String(localized: "Connect a server and choose a folder.")
        case .halted:
            return state.folders.filter { $0.state == .halted }.map { folder in
                let missing = state.problems.contains { $0.spaceId == folder.spaceId && $0.kind == .folderMissing }
                return missing
                    ? String(localized: "\(folder.spaceName) is missing.")
                    : String(localized: "\(folder.spaceName) stopped.")
            }
            .joined(separator: " ")
        case .questions:
            return state.status == .idle ? String(localized: "Everything else is up to date.") : state.status.title
        case .status(.paused):
            return String(localized: "Changes sync when you resume.")
        case .status(.offline):
            guard let server = state.account?.serverUrl else { return nil }
            let host = URL(string: server)?.host() ?? server
            return String(localized: "Retrying \(host).")
        case .status:
            return nil
        }
    }

    var headlineSymbol: (name: String, color: Color) {
        switch headline {
        case .setup: ("arrow.triangle.2.circlepath", .text2)
        case .halted: ("exclamationmark.triangle", .statusRed)
        case .questions: ("questionmark.circle", .needsYou)
        case .status(.idle): ("checkmark.circle", .statusGreen)
        case .status(.syncing), .status(.connecting): ("arrow.triangle.2.circlepath", .koi)
        case .status(.paused), .status(.stopped): ("pause", .text2)
        case .status(.offline): ("bolt.circle", .statusYellow)
        case .status(.needsSetup): ("arrow.triangle.2.circlepath", .text2)
        case .status(.halted): ("exclamationmark.triangle", .statusRed)
        }
    }
}

struct MenuContent: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow
    @Environment(\.openSettings) private var openSettings
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @AppStorage(SettingsTab.storageKey) private var settingsTab = SettingsTab.folders
    @State private var error: SessionError?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            header
            attention
            notices
            recentChanges
            separator
            actions
            separator
            MenuButton(title: "Quit Koidrive", systemImage: "power", shortcut: "⌘Q") {
                NSApplication.shared.terminate(nil)
            }
            .keyboardShortcut("q")
        }
        .padding(.horizontal, 5)
        .padding(.top, 6)
        .padding(.bottom, 5)
        .frame(width: 330)
        .tint(.koi)
    }

    private var separator: some View {
        Divider()
            .padding(.horizontal, 7)
            .padding(.vertical, 1)
    }

    private var headlineIcon: some View {
        Image(systemName: model.headlineSymbol.name)
            .font(.system(size: 15, weight: .medium))
            .foregroundStyle(model.headlineSymbol.color)
            .frame(width: 18, height: 18)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                if model.turnsHeadlineArrows(reduceMotion: reduceMotion) {
                    headlineIcon
                        .symbolEffect(.rotate.clockwise, options: .repeat(.continuous).speed(1.25))
                } else {
                    headlineIcon
                }
                Text(model.headlineTitle)
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Color.text)
            }
            if let subtitle = model.headlineSubtitle {
                Text(subtitle)
                    .font(.callout)
                    .foregroundStyle(Color.text2)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, 24)
            }
            if let progress = model.state.progress, model.headline == .status(.syncing) {
                HStack(spacing: 10) {
                    if let fraction = model.progressFraction {
                        ProgressBar(fraction: fraction)
                    }
                    Text(byteProgress(done: progress.bytesDone, total: progress.bytesTotal))
                        .font(.callout)
                        .foregroundStyle(Color.text2)
                }
                .padding(.leading, 24)
                .padding(.top, 4)
                HStack(spacing: 6) {
                    if progress.filesToUpload > 0 {
                        CountChip(title: String(localized: "\(progress.filesToUpload.formatted()) to upload"), systemImage: "arrow.up")
                    }
                    if progress.filesToDownload > 0 {
                        CountChip(title: String(localized: "\(progress.filesToDownload.formatted()) to download"), systemImage: "arrow.down")
                    }
                }
                .padding(.leading, 24)
                if let current = progress.current {
                    PathText(path: model.displayPath(current.path, spaceID: current.spaceId))
                        .font(.callout)
                        .padding(.leading, 24)
                }
            }
        }
        .padding(.horizontal, 7)
        .padding(.top, 6)
        .padding(.bottom, 4)
    }

    @ViewBuilder
    private var attention: some View {
        if !model.state.questions.isEmpty {
            AttentionRow(
                text: "\(model.state.questions.count) need you",
                systemImage: "questionmark.circle",
                tint: .needsYou,
                wash: .needsYouTint,
                count: nil
            ) {
                openWindow.show(WindowID.questions)
            }
        }
        if !model.state.problems.isEmpty {
            AttentionRow(
                text: "Review problems",
                systemImage: "exclamationmark.triangle",
                tint: .statusRed,
                wash: .redTint,
                count: model.state.problems.count
            ) {
                settingsTab = .problems
                openSettings()
                NSApp.activate()
            }
        }
    }

    @ViewBuilder
    private var notices: some View {
        ForEach(model.state.notices) { notice in
            NoticeRow(notice: notice, spaceName: model.folder(spaceID: notice.spaceId)?.spaceName) {
                Task {
                    do {
                        try await model.dismiss(notice)
                    } catch let failure as SessionError {
                        error = failure
                    } catch {}
                }
            }
        }
        if let error {
            Label {
                Text(error.message)
                    .foregroundStyle(Color.text)
            } icon: {
                Image(systemName: "exclamationmark.triangle")
                    .foregroundStyle(Color.statusRed)
            }
            .font(.callout)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 7)
        }
    }

    @ViewBuilder
    private var recentChanges: some View {
        if !model.state.recentChanges.isEmpty {
            let busy = model.headline != .status(.idle) || !model.state.notices.isEmpty || !model.state.problems.isEmpty
            VStack(alignment: .leading, spacing: 0) {
                Text("Recent changes")
                    .font(.callout.weight(.medium))
                    .foregroundStyle(Color.text2)
                    .frame(height: 24)
                ForEach(Array(model.state.recentChanges.prefix(busy ? 3 : 5).enumerated()), id: \.offset) { _, change in
                    RecentChangeRow(change: change, folder: model.folder(spaceID: change.spaceId))
                }
            }
            .padding(.horizontal, 7)
        }
    }

    private var actions: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(model.state.folders, id: \.spaceId) { folder in
                MenuButton(title: "Open \(folder.spaceName)", systemImage: "folder") {
                    NSWorkspace.shared.open(URL(filePath: folder.path, directoryHint: .isDirectory))
                }
            }
            if model.needsSetup {
                MenuButton(title: "Set Up Koidrive…", systemImage: "wand.and.stars") {
                    openWindow.show(WindowID.setup)
                }
            } else {
                MenuButton(
                    title: model.isPaused ? "Resume Sync" : "Pause Sync",
                    systemImage: model.isPaused ? "play" : "pause"
                ) {
                    do {
                        try model.togglePause()
                        error = nil
                    } catch let failure as SessionError {
                        error = failure
                    } catch {}
                }
                .disabled(model.state.status == .stopped)
            }
            MenuButton(title: "Needs You…", systemImage: "questionmark.circle") {
                openWindow.show(WindowID.questions)
            }
            Divider()
            Group {
                MenuButton(title: "Version History…", systemImage: "clock.arrow.circlepath") {
                    VersionHistoryPicker.choose(in: model.state.folders) { openWindow.showVersionHistory(of: $0) }
                }
                MenuButton(title: "Deleted Files…", systemImage: "trash") {
                    openWindow.show(WindowID.deletedFiles)
                }
            }
            .disabled(model.needsSetup)
            Divider()
            MenuButton(title: "Settings…", systemImage: "gearshape", shortcut: "⌘,") {
                openSettings()
                NSApp.activate()
            }
        }
    }

    private func byteProgress(done: UInt64, total: UInt64) -> String {
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        let units: [ByteCountFormatter.Units] = [.useBytes, .useKB, .useMB, .useGB, .useTB]
        let power = min(units.count - 1, total < 1000 ? 0 : Int(log10(Double(total)) / 3))
        formatter.allowedUnits = units[power]
        let totalText = formatter.string(fromByteCount: Int64(clamping: total))
        formatter.includesUnit = false
        let doneText = formatter.string(fromByteCount: Int64(clamping: done))
        return String(localized: "\(doneText) of \(totalText)")
    }
}

extension AppModel {
    func displayPath(_ path: String, spaceID: String) -> String {
        guard let folder = folder(spaceID: spaceID) else { return path }
        return folder.spaceName + "/" + path
    }
}

/// The system's linear progress view takes the system accent colour on macOS,
/// not the tint.
private struct ProgressBar: View {
    let fraction: Double

    var body: some View {
        Capsule()
            .fill(Color.fillStrong)
            .frame(height: 4)
            .overlay(alignment: .leading) {
                GeometryReader { geometry in
                    Capsule()
                        .fill(Color.koi)
                        .frame(width: geometry.size.width * fraction)
                }
            }
            .accessibilityElement()
            .accessibilityValue(Text(fraction, format: .percent.precision(.fractionLength(0))))
    }
}

private struct CountChip: View {
    let title: String
    let systemImage: String

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: systemImage)
                .font(.system(size: 10, weight: .semibold))
            Text(verbatim: title)
                .font(.callout.weight(.medium))
        }
        .foregroundStyle(Color.text2)
        .padding(.horizontal, 8)
        .frame(height: 22)
        .overlay(Capsule().strokeBorder(Color.fillStrong))
    }
}

private struct AttentionRow: View {
    let text: LocalizedStringKey
    let systemImage: String
    let tint: Color
    let wash: Color
    let count: Int?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                Image(systemName: systemImage)
                    .font(.system(size: 14))
                    .foregroundStyle(tint)
                    .frame(width: 16, height: 16)
                Text(text)
                    .font(.body.weight(.medium))
                    .foregroundStyle(Color.text)
                Spacer(minLength: 0)
                if let count {
                    CountBadge(count: count, fill: .redFill, text: .white)
                }
            }
            .padding(7)
            .background(RoundedRectangle(cornerRadius: 8).fill(wash))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

private struct NoticeRow: View {
    let notice: Notice
    let spaceName: String?
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: notice.symbolName)
                .font(.system(size: 14))
                .foregroundStyle(Color.text2)
                .frame(width: 16, height: 16)
            VStack(alignment: .leading, spacing: 2) {
                Text(verbatim: notice.title(spaceName: spaceName))
                    .font(.body.weight(.semibold))
                    .foregroundStyle(Color.text)
                    .fixedSize(horizontal: false, vertical: true)
                Text(notice.text)
                    .font(.callout)
                    .foregroundStyle(Color.text2)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
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
        .padding(.horizontal, 7)
        .padding(.vertical, 10)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.fillStrong))
    }
}

extension Notice {
    var symbolName: String {
        switch kind {
        case .recovery: "clock.arrow.circlepath"
        case .remoteMassDelete: "trash"
        }
    }
}

private struct RecentChangeRow: View {
    let change: RecentChange
    let folder: LinkedFolder?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: change.direction == .uploaded ? "arrow.up" : "arrow.down")
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(Color.text2)
                .frame(width: 16, alignment: .leading)
            VStack(alignment: .leading, spacing: 0) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    PathText(path: folder.map { $0.spaceName + "/" + change.path } ?? change.path)
                        .font(.body)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    Text(Date(milliseconds: change.atMs), format: .relative(presentation: .named, unitsStyle: .abbreviated))
                        .font(.callout)
                        .foregroundStyle(Color.text2)
                        .lineLimit(1)
                        .fixedSize()
                }
                Text(verbatim: change.summary)
                    .font(.callout)
                    .foregroundStyle(Color.text2)
                    .lineLimit(1)
            }
        }
        .padding(.vertical, 5)
        .contentShape(Rectangle())
        .onTapGesture(count: 2) {
            guard let folder, change.change != .deleted else { return }
            NSWorkspace.shared.activateFileViewerSelecting([URL(filePath: folder.path).appending(path: change.path)])
        }
    }
}

private struct MenuButton: View {
    let title: LocalizedStringKey
    let systemImage: String
    var shortcut: String?
    let action: () -> Void
    @State private var hovering = false
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                Image(systemName: systemImage)
                    .foregroundStyle(Color.label2)
                    .frame(width: 16)
                Text(title)
                    .foregroundStyle(Color.label)
                Spacer()
                if let shortcut {
                    Text(verbatim: shortcut)
                        .font(.caption)
                        .foregroundStyle(Color.label2)
                }
            }
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .opacity(isEnabled ? 1 : 0.4)
            .background(RoundedRectangle(cornerRadius: 6).fill(hovering && isEnabled ? Color.fillStrong : .clear))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
    }
}

#Preview("Syncing") {
    MenuContent().environment(AppModel.preview(.syncing))
}

#Preview("Questions and problems") {
    let model = AppModel.preview(.massDelete)
    _ = try? model.session.fakeCore()?.reportProblem(spaceId: nil, path: nil, kind: .signInRequired)
    return MenuContent().environment(model)
}

#Preview("Remote mass delete") {
    MenuContent().environment(AppModel.preview(.remoteMassDelete))
}

#Preview("First run") {
    MenuContent().environment(AppModel.preview(.firstRun))
}
