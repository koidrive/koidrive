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
    public let question: Question
    public let original: ConflictFile
    public let copy: ConflictFile
    public private(set) var phase: Phase
    @ObservationIgnored private let model: AppModel

    /// Nil unless `question` is a conflict.
    public init?(question: Question, model: AppModel, phase: Phase = .choosing) {
        guard case .conflict(let original, let copy) = question.kind else { return nil }
        let folder = model.folder(spaceID: question.spaceId).map { URL(filePath: $0.path, directoryHint: .isDirectory) }
        id = question.id
        self.question = question
        self.original = ConflictFile(role: .original, side: original, url: folder?.appending(path: original.path))
        self.copy = ConflictFile(role: .copy, side: copy, url: folder?.appending(path: copy.path))
        self.model = model
        self.phase = phase
    }

    public var isKeeping: Bool {
        if case .keeping = phase { true } else { false }
    }

    public func keep(_ answer: Answer) async {
        guard !isKeeping else { return }
        phase = .keeping(answer)
        do {
            try await model.answer(question, with: answer)
            phase = .settled(answer)
        } catch let error as SessionError {
            phase = .failed(error)
        } catch {
            phase = .failed(.NotSaved(reason: error.localizedDescription))
        }
    }
}

extension ConflictSide {
    public var name: String { (path as NSString).lastPathComponent }
}
