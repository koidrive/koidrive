// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import QuickLook
import QuickLookThumbnailing
import SwiftUI

struct ConflictDetail: View {
    @Environment(AppModel.self) private var model
    let resolution: ConflictResolution
    var thumbnails: FileThumbnail.Phase?
    @State private var previewURL: URL?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 12) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(resolution.question.title)
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(Color.text)
                    Text(resolution.question.explanation)
                        .foregroundStyle(Color.text2)
                        .fixedSize(horizontal: false, vertical: true)
                }
                GroupBand(title: "Versions") {
                    Text("Asked \(Date(milliseconds: resolution.question.askedAtMs), format: .relative(presentation: .named))")
                        .foregroundStyle(Color.text2)
                }
                HStack(alignment: .top, spacing: 12) {
                    ConflictCard(file: resolution.original, original: resolution.original, thumbnail: thumbnails, previewURL: $previewURL) {
                        keep(resolution.original.answer)
                    }
                    ConflictCard(file: resolution.copy, original: resolution.original, thumbnail: thumbnails, previewURL: $previewURL) {
                        keep(resolution.copy.answer)
                    }
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 16)
            footer
        }
        .disabled(resolution.isKeeping)
        .quickLookPreview($previewURL)
    }

    private var footer: some View {
        HStack(spacing: 8) {
            switch resolution.phase {
            case .keeping(let answer):
                ProgressView().controlSize(.small)
                Text(answer.keepingTitle)
                    .foregroundStyle(Color.text2)
            case .failed(let error):
                ErrorLine(message: String(localized: "Couldn't answer. \(error.message)"))
            case .choosing, .settled:
                if model.state.status == .offline {
                    Label {
                        Text("Offline. Your choice applies when Koidrive reconnects.")
                            .foregroundStyle(Color.text)
                    } icon: {
                        Image(systemName: "wifi.slash").foregroundStyle(Color.statusYellow)
                    }
                } else {
                    Label("Keep Both leaves both files as they are.", systemImage: "info.circle")
                        .foregroundStyle(Color.text2)
                }
            }
            Spacer(minLength: 0)
            Button("Keep Both") { keep(.keepBoth) }
        }
        .lineLimit(1)
        .padding(.horizontal, 20)
        .frame(height: 56)
        .overlay(alignment: .top) { Color.line.frame(height: 1) }
    }

    private func keep(_ answer: Answer) {
        Task { await resolution.keep(answer) }
    }
}

private struct ConflictCard: View {
    let file: ConflictFile
    let original: ConflictFile
    @Binding var previewURL: URL?
    let keep: () -> Void
    @State private var thumbnail: FileThumbnail.Phase
    private let loads: Bool

    init(file: ConflictFile, original: ConflictFile, thumbnail: FileThumbnail.Phase?, previewURL: Binding<URL?>,
         keep: @escaping () -> Void) {
        self.file = file
        self.original = original
        _thumbnail = State(initialValue: thumbnail ?? .loading)
        loads = thumbnail == nil
        _previewURL = previewURL
        self.keep = keep
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            FileThumbnail(url: file.url, phase: $thumbnail, loads: loads)
                .frame(height: 112)
                .frame(maxWidth: .infinity)
                .background(RoundedRectangle(cornerRadius: 6).fill(Color.group))
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(Text("Preview of \(file.name)"))
            HStack(spacing: 8) {
                Image(systemName: "doc.text")
                    .foregroundStyle(Color.text)
                    .frame(width: 16)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 0) {
                    Text(verbatim: file.name)
                        .fontWeight(.semibold)
                        .foregroundStyle(Color.text)
                    Text(file.role == .original ? "Original" : "Conflicted copy")
                        .font(.callout)
                        .foregroundStyle(Color.text2)
                }
                .lineLimit(1)
                .truncationMode(.middle)
            }
            Grid(alignment: .leading, horizontalSpacing: 6, verticalSpacing: 2) {
                GridRow {
                    Text("Modified").foregroundStyle(Color.text2).frame(width: 64, alignment: .leading)
                    Text(verbatim: conflictDate(Date(milliseconds: file.side.modifiedAtMs))).foregroundStyle(Color.text)
                }
                GridRow {
                    Text("Size").foregroundStyle(Color.text2).frame(width: 64, alignment: .leading)
                    Text(Int64(clamping: file.side.size), format: .byteCount(style: .file)).foregroundStyle(Color.text)
                }
            }
            PathText(path: file.side.path)
                .help(file.side.path)
            Text(file.role == .original
                 ? String(localized: "Keeps this file. The copy moves to Deleted Files.")
                 : String(localized: "Puts this content in \(original.name). Its old content stays in Version History."))
                .font(.callout)
                .foregroundStyle(Color.text2)
                .fixedSize(horizontal: false, vertical: true)
                .frame(minHeight: 32, alignment: .topLeading)
            HStack(spacing: 8) {
                Button("Quick Look") { previewURL = file.url }
                    .controlSize(.small)
                    .disabled(thumbnail != .ready)
                Spacer(minLength: 0)
                Button(file.role == .original ? "Keep Original" : "Keep Copy", action: keep)
            }
            .padding(.top, 2)
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.content))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.separator))
    }
}

struct FileThumbnail: View {
    enum Phase: Equatable {
        case loading
        case ready
        case none
    }

    let url: URL?
    @Binding var phase: Phase
    var loads = true
    @Environment(\.displayScale) private var scale
    @State private var image: NSImage?

    var body: some View {
        Group {
            switch phase {
            case .ready:
                if let image {
                    Image(nsImage: image)
                        .resizable()
                        .aspectRatio(contentMode: .fit)
                        .padding(.vertical, 10)
                        .padding(.horizontal, 14)
                }
            case .loading:
                VStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Loading preview…")
                }
            case .none:
                VStack(spacing: 6) {
                    Image(systemName: "doc")
                    Text("No preview for this file type")
                }
            }
        }
        .font(.callout)
        .foregroundStyle(Color.text2)
        .task(id: url) { if loads { await load() } }
    }

    private func load() async {
        guard let url else {
            phase = .none
            return
        }
        phase = .loading
        let request = QLThumbnailGenerator.Request(fileAt: url, size: CGSize(width: 320, height: 92), scale: scale,
                                                   representationTypes: .thumbnail)
        if let thumbnail = try? await QLThumbnailGenerator.shared.generateBestRepresentation(for: request) {
            image = thumbnail.nsImage
            phase = .ready
        } else {
            phase = .none
        }
    }
}

struct SettledConflict: View {
    @Environment(AppModel.self) private var model
    let resolution: ConflictResolution
    let answer: Answer

    var body: some View {
        EmptyState(systemImage: "checkmark", title: title, message: Text(message)) {
            Button("Show in Finder") {
                let urls = answer == .keepBoth ? [resolution.original.url, resolution.copy.url] : [resolution.original.url]
                NSWorkspace.shared.activateFileViewerSelecting(urls.compactMap { $0 })
            }
            .controlSize(.small)
            .disabled(resolution.original.url == nil)
        }
        .frame(maxWidth: 420)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var title: LocalizedStringKey {
        switch answer {
        case .keepCopy: "\(resolution.original.name) now has the copy's content"
        case .keepBoth: "Both files are kept"
        default: "\(resolution.original.name) is kept"
        }
    }

    private var message: String {
        switch answer {
        case .keepCopy: String(localized: "Its old content is in Version History. The copy moved to Deleted Files.")
        case .keepBoth:
            if let folder = model.folder(spaceID: resolution.question.spaceId) {
                String(localized: "Both keep syncing in \(folder.spaceName).")
            } else {
                String(localized: "Both keep syncing.")
            }
        default: String(localized: "The copy moved to Deleted Files.")
        }
    }
}

private extension Answer {
    var keepingTitle: String {
        switch self {
        case .keepOriginal: String(localized: "Keeping the original…")
        case .keepCopy: String(localized: "Keeping the copy…")
        default: String(localized: "Keeping both…")
        }
    }
}

func conflictDate(_ date: Date) -> String {
    let time = date.formatted(.dateTime.hour().minute())
    if Calendar.current.isDateInToday(date) { return String(localized: "Today, \(time)") }
    if Calendar.current.isDateInYesterday(date) { return String(localized: "Yesterday, \(time)") }
    return date.formatted(.dateTime.day().month(.abbreviated).hour().minute())
}
