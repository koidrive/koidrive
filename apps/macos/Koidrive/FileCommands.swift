// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

@MainActor
enum VersionHistoryPicker {
    static func choose(in folders: [LinkedFolder], then open: @escaping @MainActor (URL) -> Void) {
        let (panel, filter) = makePanel(in: folders)
        NSApp.activate()
        panel.begin { response in
            withExtendedLifetime(filter) {}
            guard response == .OK, let url = panel.url else { return }
            open(url)
        }
    }

    static func makePanel(in folders: [LinkedFolder]) -> (NSOpenPanel, NSOpenSavePanelDelegate) {
        let panel = NSOpenPanel()
        let filter = LinkedFolderFilter(folders: folders)
        panel.delegate = filter
        panel.title = String(localized: "Version History")
        panel.message = String(localized: "Choose a file in \(folders.map(\.spaceName).formatted(.list(type: .or))) to see its versions.")
        panel.prompt = String(localized: "Show Versions")
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.directoryURL = folders.first?.url
        return (panel, filter)
    }
}

private final class LinkedFolderFilter: NSObject, NSOpenSavePanelDelegate {
    private let roots: [String]

    init(folders: [LinkedFolder]) {
        roots = folders.map { resolvedPath($0.url) }
    }

    func panel(_ sender: Any, shouldEnable url: URL) -> Bool {
        let path = resolvedPath(url)
        if roots.contains(where: { path.hasPrefix($0 + "/") }) { return true }
        let isDirectory = (try? url.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true
        return isDirectory && roots.contains { $0 == path || $0.hasPrefix(path == "/" ? path : path + "/") }
    }
}

private func resolvedPath(_ url: URL) -> String {
    var path = url.resolvingSymlinksInPath().standardizedFileURL.path(percentEncoded: false)
    while path.count > 1, path.hasSuffix("/") { path.removeLast() }
    return path
}

enum DropTarget: CaseIterable, Identifiable {
    case versionHistory

    var id: Self { self }

    var title: String {
        switch self {
        case .versionHistory: String(localized: "Version History")
        }
    }

    var subtitle: String {
        switch self {
        case .versionHistory: String(localized: "See versions")
        }
    }

    var systemImage: String {
        switch self {
        case .versionHistory: "clock.arrow.circlepath"
        }
    }
}

struct DropPanel: View {
    let targets: [DropTarget]
    let drop: (DropTarget, URL) -> Void
    @State private var targeted: DropTarget?

    init(targets: [DropTarget], targeted: DropTarget? = nil, drop: @escaping (DropTarget, URL) -> Void) {
        self.targets = targets
        self.drop = drop
        _targeted = State(initialValue: targeted)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Drop on \(targets.map(\.title).formatted(.list(type: .or)))")
                .font(.callout.weight(.medium))
                .foregroundStyle(Color.text2)
                .padding(.horizontal, 4)
                .padding(.top, 18)
            HStack(spacing: 8) {
                ForEach(targets) { target in
                    VStack(spacing: 6) {
                        Image(systemName: target.systemImage)
                            .font(.system(size: 18))
                        Text(verbatim: target.title)
                            .fontWeight(.semibold)
                        Text(verbatim: target.subtitle)
                            .font(.callout)
                            .foregroundStyle(Color.text2)
                    }
                    .foregroundStyle(Color.text)
                    .frame(maxWidth: .infinity)
                    .frame(height: 96)
                    .overlay {
                        if targeted != target {
                            RoundedRectangle(cornerRadius: 8).strokeBorder(Color.separatorLine)
                        }
                    }
                    .selectedRow(targeted == target)
                    .accessibilityElement(children: .combine)
                    .dropDestination(for: URL.self) { urls, _ in
                        guard let url = urls.first else { return false }
                        drop(target, url)
                        return true
                    } isTargeted: { inside in
                        if inside {
                            targeted = target
                        } else if targeted == target {
                            targeted = nil
                        }
                    }
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.top, 14)
        .padding(.bottom, 12)
        .frame(width: 330)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Color.separatorLine, lineWidth: 0.5))
    }
}

@MainActor
final class MenuBarDropTarget: NSView {
    private var isEnabled: () -> Bool = { true }
    private var perform: (DropTarget, URL) -> Void = { _, _ in }
    private var panel: NSPanel?
    private var watch: Timer?

    static func install(isEnabled: @escaping () -> Bool, perform: @escaping (DropTarget, URL) -> Void) -> Bool {
        guard let button = NSApp.windows.lazy.compactMap({ $0.contentView.flatMap(statusButton(in:)) }).first else { return false }
        let target = button.subviews.lazy.compactMap { $0 as? MenuBarDropTarget }.first ?? {
            let target = MenuBarDropTarget(frame: button.bounds)
            target.autoresizingMask = [.width, .height]
            target.registerForDraggedTypes([.fileURL])
            button.addSubview(target)
            return target
        }()
        target.isEnabled = isEnabled
        target.perform = perform
        return true
    }

    private static func statusButton(in view: NSView) -> NSStatusBarButton? {
        if let button = view as? NSStatusBarButton { return button }
        return view.subviews.lazy.compactMap(statusButton(in:)).first
    }

    override func mouseDown(with event: NSEvent) {
        superview?.mouseDown(with: event)
    }

    override func rightMouseDown(with event: NSEvent) {
        superview?.rightMouseDown(with: event)
    }

    override func draggingEntered(_ sender: any NSDraggingInfo) -> NSDragOperation {
        if isEnabled() { showPanel() }
        return []
    }

    private func showPanel() {
        guard panel == nil, let anchor = window?.frame else { return }
        let host = NSHostingView(rootView: DropPanel(targets: DropTarget.allCases) { [weak self] target, url in
            self?.perform(target, url)
            self?.closePanel()
        })
        let size = host.fittingSize
        let screen = window?.screen?.visibleFrame ?? NSScreen.main?.visibleFrame ?? anchor
        let x = min(max(anchor.midX - size.width / 2, screen.minX + 6), screen.maxX - size.width - 6)
        let panel = NSPanel(
            contentRect: NSRect(x: x, y: anchor.minY - 6 - size.height, width: size.width, height: size.height),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        panel.level = .popUpMenu
        panel.backgroundColor = .clear
        panel.isOpaque = false
        panel.hasShadow = true
        panel.hidesOnDeactivate = false
        panel.isReleasedWhenClosed = false
        panel.contentView = host
        panel.orderFrontRegardless()
        self.panel = panel
        watch = Timer.scheduledTimer(withTimeInterval: 0.2, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.closeAfterTheDrag() }
        }
    }

    private func closeAfterTheDrag() {
        guard NSEvent.pressedMouseButtons & 1 == 0 else { return }
        watch?.invalidate()
        watch = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated { self?.closePanel() }
        }
    }

    private func closePanel() {
        watch?.invalidate()
        watch = nil
        panel?.close()
        panel = nil
    }
}

struct MenuBarDrop: ViewModifier {
    let model: AppModel
    @Environment(\.openWindow) private var openWindow

    func body(content: Content) -> some View {
        content.task {
            for _ in 0..<40 {
                let installed = MenuBarDropTarget.install(isEnabled: { !model.needsSetup }) { target, url in
                    switch target {
                    case .versionHistory: openWindow.showVersionHistory(of: url)
                    }
                }
                if installed { return }
                try? await Task.sleep(for: .milliseconds(250))
            }
        }
    }
}
