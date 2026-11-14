// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

struct QuestionsView: View {
    @Environment(AppModel.self) private var model
    @State private var selection: String?
    @State private var conflict: ConflictResolution?
    @State private var answering: [String: ConflictResolution] = [:]
    private let thumbnails: FileThumbnail.Phase?

    init(conflict: ConflictResolution? = nil, thumbnails: FileThumbnail.Phase? = nil) {
        _selection = State(initialValue: conflict?.id)
        _conflict = State(initialValue: conflict)
        self.thumbnails = thumbnails
    }

    var body: some View {
        NavigationSplitView {
            ScrollViewReader { proxy in
                ScrollView {
                    VStack(alignment: .leading, spacing: 2) {
                        ForEach(model.questionSections) { section in
                            if model.questionSectionsShowSpace {
                                SpaceHeading(spaceID: section.spaceID, folder: section.folder)
                                    .font(.callout.weight(.semibold))
                                    .foregroundStyle(Color.text2)
                                    .lineLimit(1)
                                    .padding(.horizontal, 8)
                                    .padding(.bottom, 4)
                                    .frame(maxWidth: .infinity, minHeight: 28, alignment: .bottomLeading)
                            }
                            rows(of: section)
                        }
                    }
                    .padding(.vertical, 10)
                    .padding(.horizontal, 8)
                }
                .focusable()
                .focusEffectDisabled()
                .onMoveCommand { direction in
                    selection = moved(selection, in: model.questionSections.flatMap(\.questions).map(\.id), direction)
                }
                .onChange(of: selection) { _, id in
                    if let id { proxy.scrollTo(id) }
                }
            }
            .navigationSplitViewColumnWidth(min: 240, ideal: 260)
        } detail: {
            Group {
                if let conflict, case .settled(let answer) = conflict.phase {
                    SettledConflict(resolution: conflict, answer: answer)
                } else if let conflict, conflict.id == selection {
                    ConflictDetail(resolution: conflict, thumbnails: thumbnails)
                        .id(conflict.id)
                        .frame(minWidth: 560, minHeight: 360)
                } else if let question = model.state.questions.first(where: { $0.id == selection }) {
                    QuestionDetail(question: question, folder: model.folder(spaceID: question.spaceId))
                        .id(question.id)
                        .frame(minWidth: 480, minHeight: 360)
                } else {
                    EmptyState(
                        systemImage: "checkmark.circle",
                        title: "Nothing needs you",
                        message: Text("Decisions Koidrive needs from you appear here.")
                    )
                }
            }
            .background(Color.panel)
            .buttonStyle(.secondary)
        }
        .buttonStyle(.automatic)
        .navigationTitle("Needs You")
        .onAppear(perform: seen)
        .onChange(of: model.unseenQuestionIDs) { seen() }
        .onChange(of: model.state.questions.map(\.id), initial: true) { _, ids in
            if let selection, ids.contains(selection) || conflictIsAnswered(selection) { return }
            selection = model.questionSections.first?.questions.first?.id
        }
        .onChange(of: selection, initial: true) { _, id in
            guard conflict?.id != id else { return }
            if let conflict { answering[conflict.id] = conflict }
            answering = answering.filter { $0.value.isKeeping }
            conflict = id.flatMap { answering[$0] }
                ?? model.state.questions.first { $0.id == id }.flatMap { ConflictResolution(question: $0, model: model) }
        }
    }

    @ViewBuilder
    private func rows(of section: QuestionSection) -> some View {
        if let cause = section.recoveryCause {
            HStack(spacing: 6) {
                Image(systemName: "clock")
                    .font(.system(size: 10))
                    .foregroundStyle(Color.text3)
                Text(cause.shortText)
                    .font(.callout)
                    .foregroundStyle(Color.text2)
                    .lineLimit(1)
            }
            .padding(.horizontal, 8)
            .frame(height: 24)
        }
        ForEach(section.questions) { question in
            Button { selection = question.id } label: {
                QuestionRow(question: question, selected: selection == question.id)
            }
            .buttonStyle(.plain)
        }
    }

    private func conflictIsAnswered(_ id: String) -> Bool {
        guard let conflict, conflict.id == id else { return false }
        switch conflict.phase {
        case .keeping, .settled: return true
        case .choosing, .failed: return false
        }
    }

    private func seen() {
        model.markQuestionsSeen()
    }
}

private func moved(_ id: String?, in ids: [String], _ direction: MoveCommandDirection) -> String? {
    guard let index = id.flatMap(ids.firstIndex(of:)) else { return ids.first }
    switch direction {
    case .up: return ids[max(index - 1, 0)]
    case .down: return ids[min(index + 1, ids.count - 1)]
    default: return id
    }
}

struct SpaceHeading: View {
    let spaceID: String?
    let folder: LinkedFolder?

    var body: some View {
        if let folder {
            Text(folder.spaceName)
        } else if let spaceID {
            HStack(spacing: 6) {
                Text("Unlinked space")
                Text(verbatim: spaceID)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        } else {
            Text(verbatim: "Koidrive")
        }
    }
}

private struct QuestionRow: View {
    let question: Question
    let selected: Bool

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: question.symbolName)
                .foregroundStyle(selected ? Color.koiText : Color.text2)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 0) {
                Text(question.listTitle)
                    .fontWeight(selected ? .semibold : .regular)
                    .foregroundStyle(Color.text)
                Text(question.counts)
                    .font(.callout)
                    .foregroundStyle(Color.text2)
            }
            .lineLimit(1)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8)
        .frame(height: 44)
        .background {
            if selected { RoundedRectangle(cornerRadius: 6).fill(Color.fillStrong) }
        }
        .contentShape(Rectangle())
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

struct QuestionDetail: View {
    @Environment(AppModel.self) private var model
    let question: Question
    let folder: LinkedFolder?
    @State private var confirming: Answer?
    @State private var error: SessionError?
    @State private var answering: Bool
    @State private var selectedItemID: String?

    init(question: Question, folder: LinkedFolder?, error: SessionError? = nil, answering: Bool = false) {
        self.question = question
        self.folder = folder
        _error = State(initialValue: error)
        _answering = State(initialValue: answering)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 12) {
                Text(question.title)
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Color.text)
                Text(question.explanation)
                    .foregroundStyle(Color.text2)
                    .fixedSize(horizontal: false, vertical: true)
                if let cause = question.recoveryCause {
                    Label(cause.text, systemImage: "clock")
                        .foregroundStyle(Color.text2)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                        .background(RoundedRectangle(cornerRadius: 6).fill(Color.group))
                }
                HStack {
                    Text(question.itemsHeading)
                        .fontWeight(.semibold)
                        .foregroundStyle(Color.text)
                    Spacer()
                    Text("Asked \(Date(milliseconds: question.askedAtMs), format: .relative(presentation: .named))")
                        .font(.callout)
                        .foregroundStyle(Color.text2)
                }
            }
            .padding([.horizontal, .top], 20)
            .padding(.bottom, 8)
            items
            if let selectedItem {
                HStack(spacing: 6) {
                    Text("Selected path:")
                        .font(.callout)
                        .foregroundStyle(Color.text2)
                    Text(verbatim: fullPath(selectedItem))
                        .font(.subheadline)
                        .foregroundStyle(Color.text)
                        .textSelection(.enabled)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    Button("Copy Path") { copy(fullPath(selectedItem)) }
                        .buttonStyle(.plain)
                        .font(.callout.weight(.medium))
                        .foregroundStyle(Color.text)
                }
                .padding(.horizontal, 20)
                .padding(.top, 8)
                .padding(.bottom, 12)
            } else {
                Spacer(minLength: 12)
                    .frame(maxHeight: 12)
            }
            footer
        }
        .onAppear { selectedItemID = question.items.first?.nodeId }
        .sheet(item: $confirming) { answer in
            ConfirmSheet(
                title: question.confirmation(for: answer),
                message: question.confirmationMessage(for: answer),
                action: question.title(for: answer),
                confirm: { confirming = nil; send(answer) },
                cancel: { confirming = nil }
            )
        }
    }

    private var items: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(spacing: 2) {
                    ForEach(question.items, id: \.nodeId) { item in
                        Button { selectedItemID = item.nodeId } label: {
                            HeldItemRow(item: item, selected: selectedItemID == item.nodeId)
                        }
                        .buttonStyle(.plain)
                        .help(item.path)
                        .contextMenu {
                            Button("Copy Path") { copy(fullPath(item)) }
                        }
                    }
                }
                .padding(4)
            }
            .focusable()
            .focusEffectDisabled()
            .onMoveCommand { direction in
                selectedItemID = moved(selectedItemID, in: question.items.map(\.nodeId), direction)
            }
            .onChange(of: selectedItemID) { _, id in
                if let id { proxy.scrollTo(id) }
            }
        }
        .onCopyCommand {
            selectedItem.map { [NSItemProvider(object: fullPath($0) as NSString)] } ?? []
        }
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color.line))
        .frame(minHeight: 80, maxHeight: .infinity)
        .padding(.horizontal, 20)
    }

    private var footer: some View {
        HStack(spacing: 8) {
            if let error {
                HStack(spacing: 6) {
                    Image(systemName: "exclamationmark.triangle")
                        .foregroundStyle(Color.statusRed)
                    Text(error.message)
                        .font(.callout)
                        .foregroundStyle(Color.text)
                        .lineLimit(2)
                        .fixedSize(horizontal: false, vertical: true)
                }
            } else {
                Label(question.scope, systemImage: "questionmark.circle")
                    .foregroundStyle(Color.text2)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
            if answering {
                ProgressView().controlSize(.small)
            }
            ForEach(orderedAnswers, id: \.self) { answer in
                if answer == primaryAnswer {
                    Button(question.title(for: answer)) { choose(answer) }
                        .buttonStyle(.primary)
                } else {
                    Button(question.title(for: answer), role: answer.isDestructive ? .destructive : nil) { choose(answer) }
                }
            }
        }
        .disabled(answering)
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
        .background(Color(nsColor: .windowBackgroundColor))
        .overlay(alignment: .top) { Color.line.frame(height: 1) }
    }

    private var primaryAnswer: Answer? {
        question.answers.first { !$0.isDestructive }
    }

    private var orderedAnswers: [Answer] {
        question.answers.filter { $0 != primaryAnswer } + [primaryAnswer].compactMap { $0 }
    }

    private var selectedItem: HeldItem? {
        guard let selectedItemID else { return nil }
        return question.items.first { $0.nodeId == selectedItemID }
    }

    private func fullPath(_ item: HeldItem) -> String {
        guard let folder else { return item.path }
        return abbreviatingHome(URL(filePath: folder.path).appending(path: item.path).path(percentEncoded: false))
    }

    private func copy(_ path: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(path, forType: .string)
    }

    private func choose(_ answer: Answer) {
        if answer.isDestructive {
            confirming = answer
        } else {
            send(answer)
        }
    }

    private func send(_ answer: Answer) {
        answering = true
        error = nil
        Task {
            do {
                try await model.answer(question, with: answer)
            } catch let failure as SessionError {
                error = failure
            } catch {}
            answering = false
        }
    }
}

extension Answer: @retroactive Identifiable {
    public var id: Self { self }
}

struct ConfirmSheet: View {
    let title: String
    let message: String
    let action: String
    let confirm: () -> Void
    let cancel: () -> Void

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 32))
                .foregroundStyle(Color.statusRed)
            Text(title)
                .fontWeight(.semibold)
                .foregroundStyle(Color.text)
                .multilineTextAlignment(.center)
            Text(message)
                .foregroundStyle(Color.text2)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .frame(width: 240)
            VStack(spacing: 8) {
                Button(action: confirm) {
                    Text(action).frame(maxWidth: .infinity)
                }
                .buttonStyle(.destructiveConfirm)
                Button(action: cancel) {
                    Text("Cancel").frame(maxWidth: .infinity)
                }
                .keyboardShortcut(.defaultAction)
                .keyboardShortcut(.cancelAction)
            }
            .padding(.top, 4)
        }
        .padding(20)
        .frame(width: 280)
    }
}

private struct HeldItemRow: View {
    let item: HeldItem
    let selected: Bool

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: item.kind == .folder ? "folder" : "doc")
                .foregroundStyle(Color.text2)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 1) {
                PathText(path: item.path)
                if let detail = item.detail.text {
                    Text(detail)
                        .font(.callout)
                        .foregroundStyle(Color.text2)
                        .lineLimit(1)
                }
            }
            Spacer(minLength: 0)
            Text(verbatim: fileCount(item.files))
                .font(.callout)
                .foregroundStyle(Color.text2)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .frame(minHeight: 36)
        .background {
            if selected { RoundedRectangle(cornerRadius: 6).fill(Color.group) }
        }
        .contentShape(Rectangle())
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

#Preview("Mass delete") {
    QuestionsView().environment(AppModel.preview(.massDelete))
}

#Preview("One linked space") {
    let model = AppModel.preview(.massDelete)
    try? model.session.unlinkFolder(spaceId: "space-photos")
    return QuestionsView().environment(model)
}

#Preview("Recovery") {
    QuestionsView().environment(AppModel.preview(.recovery))
}

#Preview("Two spaces: many questions") {
    QuestionsView().environment(AppModel.preview(.manyQuestions))
}

#Preview("None") {
    QuestionsView().environment(AppModel.preview(.upToDate))
}
