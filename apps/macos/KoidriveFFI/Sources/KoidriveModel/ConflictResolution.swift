// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import Foundation
import Observation

/// One file of a conflict, located in its linked folder.
public struct ConflictFile: Equatable, Sendable {
    public enum Role: Equatable, Sendable {
        case original
        case copy
    }

    public let role: Role
    public let side: ConflictSide
    /// The file on this Mac, or nil when its space has no linked folder here.
    public let url: URL?

    public init(role: Role, side: ConflictSide, url: URL?) {
        self.role = role
        self.side = side
        self.url = url
    }

    public var name: String { side.name }

    public var directory: String {
        String(side.path.dropLast(name.count))
    }

    public var answer: Answer { role == .original ? .keepOriginal : .keepCopy }
}

/// A conflict question and the person's progress through answering it.
@MainActor
@Observable
public final class ConflictResolution: Identifiable {
    public enum Phase: Equatable, Sendable {
        case choosing
        case keeping(Answer)
        case failed(SessionError)
        case settled(Answer)
    }

    public nonisolated let id: String
    public private(set) var phase: Phase
    @ObservationIgnored private var lastSeen: Question
    @ObservationIgnored private let model: AppModel

    /// Nil unless `question` is a conflict.
    public init?(question: Question, model: AppModel, phase: Phase = .choosing) {
        guard case .conflict = question.kind else { return nil }
        id = question.id
        lastSeen = question
        self.model = model
        self.phase = phase
    }

    /// The question as the session reports it now, or as it was last seen once it is answered.
    public var question: Question {
        model.state.questions.first { $0.id == id && $0.isConflict } ?? lastSeen
    }

    public var original: ConflictFile { file(.original) }
    public var copy: ConflictFile { file(.copy) }

    private func file(_ role: ConflictFile.Role) -> ConflictFile {
        let question = question
        guard case .conflict(let original, let copy) = question.kind else { preconditionFailure("a conflict") }
        let side = role == .original ? original : copy
        let folder = model.folder(spaceID: question.spaceId)?.url
        return ConflictFile(role: role, side: side, url: folder?.appending(path: side.path))
    }

    public var isKeeping: Bool {
        if case .keeping = phase { true } else { false }
    }

    public func keep(_ answer: Answer) async {
        guard !isKeeping else { return }
        phase = .keeping(answer)
        lastSeen = question
        do {
            try await model.answer(lastSeen, with: answer)
            phase = .settled(answer)
        } catch let error as SessionError {
            phase = .failed(error)
        } catch {
            phase = .failed(.NotSaved(reason: error.localizedDescription))
        }
    }
}

private extension Question {
    var isConflict: Bool {
        if case .conflict = kind { true } else { false }
    }
}

extension ConflictSide {
    public var name: String { (path as NSString).lastPathComponent }
}
