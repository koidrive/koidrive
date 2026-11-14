// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import Foundation
import Testing

import KoidriveModel

@MainActor
@Suite struct ConflictResolutionTests {
    private func conflict() throws -> (AppModel, ConflictResolution) {
        let model = AppModel.fake(scenario: .conflict)
        model.start()
        let question = try #require(model.state.questions.first)
        return (model, try #require(ConflictResolution(question: question, model: model)))
    }

    @Test func locatesBothFilesInTheLinkedFolder() throws {
        let (model, resolution) = try conflict()
        let folder = try #require(model.folder(spaceID: resolution.question.spaceId))
        #expect(resolution.original.role == .original)
        #expect(resolution.original.name == "Budget 2027.xlsx")
        #expect(resolution.original.directory == "")
        #expect(resolution.original.url?.path(percentEncoded: false) == folder.path + "/Budget 2027.xlsx")
        #expect(resolution.copy.role == .copy)
        #expect(resolution.copy.url?.deletingLastPathComponent().path(percentEncoded: false) == folder.path + "/")
        #expect(resolution.original.answer == .keepOriginal)
        #expect(resolution.copy.answer == .keepCopy)
        #expect(resolution.phase == .choosing)
    }

    @Test func aFileInASubfolderNamesItsDirectory() {
        let side = ConflictSide(nodeId: "n", path: "Finance/2027/Budget.xlsx", size: 1, modifiedAtMs: 0)
        let file = ConflictFile(role: .copy, side: side, url: nil)
        #expect(file.name == "Budget.xlsx")
        #expect(file.directory == "Finance/2027/")
    }

    @Test func onlyAConflictMakesAResolution() throws {
        let model = AppModel.fake(scenario: .massDelete)
        let question = try #require(model.state.questions.first)
        #expect(ConflictResolution(question: question, model: model) == nil)
    }

    @Test func keepingSettlesAndAnswersTheQuestion() async throws {
        let (model, resolution) = try conflict()
        await resolution.keep(.keepCopy)
        #expect(resolution.phase == .settled(.keepCopy))
        #expect(model.state.questions.isEmpty)
    }

    @Test func aStaleQuestionFailsWithItsReasonAndCanBeAnsweredNoMore() async throws {
        let (model, resolution) = try conflict()
        try await model.answer(resolution.question, with: .keepOriginal)
        await resolution.keep(.keepBoth)
        #expect(resolution.phase == .failed(.QuestionNotFound))
    }

    @Test func aFailedAnswerCanBeTriedAgain() async throws {
        let (model, resolution) = try conflict()
        model.session.fakeCore()?.failNext(error: .ServerUnreachable(reason: "offline"))
        await resolution.keep(.keepBoth)
        #expect(resolution.phase == .failed(.ServerUnreachable(reason: "offline")))
        await resolution.keep(.keepBoth)
        #expect(resolution.phase == .settled(.keepBoth))
    }

    @Test func aSecondAnswerWhileOneIsSentIsIgnored() async throws {
        let (_, resolution) = try conflict()
        async let first: Void = resolution.keep(.keepOriginal)
        await Task.yield()
        #expect(resolution.isKeeping)
        await resolution.keep(.keepCopy)
        await first
        #expect(resolution.phase == .settled(.keepOriginal))
    }
}
