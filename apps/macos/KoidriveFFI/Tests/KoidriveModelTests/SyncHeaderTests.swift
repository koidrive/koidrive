// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import KoidriveModel
import Testing

private func folder(_ name: String, _ activity: FolderActivity?, state: FolderState = .syncing, moved: UInt64 = 0) -> LinkedFolder {
    LinkedFolder(spaceId: "space-\(name)", spaceName: name, path: "/Users/ada/Koidrive/\(name)", state: state, activity: activity, bytesMoved: moved)
}

private func transfers(up: UInt64 = 0, down: UInt64 = 0, done: UInt64, total: UInt64, current: String? = nil) -> SyncProgress {
    SyncProgress(filesToUpload: up, filesToDownload: down, bytesDone: done, bytesTotal: total,
                 current: current.map { FolderPath(spaceId: "space-Documents", path: $0) })
}

@MainActor
private func syncing(_ folders: [LinkedFolder], _ progress: SyncProgress? = nil, scenario: FakeScenario = .upToDate) -> AppModel {
    let model = AppModel.fake(scenario: scenario)
    model.apply(.status(status: .syncing))
    model.apply(.folders(folders: folders))
    model.apply(.progress(progress: progress))
    return model
}

@MainActor
@Suite struct SyncHeaderTests {
    @Test func aFirstScanSlidesTheBarAndCountsTheFilesFound() {
        let model = syncing([folder("Photos", .scanning(files: 12_408)), folder("Documents", nil, state: .upToDate)])
        #expect(model.headline == .status(.syncing))
        let header = model.syncHeader
        #expect(header.folders == 1)
        #expect(header.activity == .scanning(folder: "Photos", files: nil))
        #expect(header.meter == Meter(fill: .sliding, caption: .filesFound(12_408)))
        #expect(header.note == nil)
    }

    @Test func activitiesWithoutACountSlideTheBarAndNameTheFolder() {
        let expected: [(FolderActivity, ActivityLine)] = [
            (.listing, .listing(folder: "Photos")),
            (.applying(changes: 84), .applying(folder: "Photos", changes: 84)),
            (.catchingUp, .finishingUp(folder: "Photos")),
            (.checking(files: 3_120, bytes: 0), .comparing(folder: "Photos", files: 3_120)),
        ]
        for (activity, line) in expected {
            let header = syncing([folder("Photos", activity)]).syncHeader
            #expect(header.activity == line, "\(activity)")
            #expect(header.meter == Meter(fill: .sliding), "\(activity)")
        }
    }

    @Test func comparingFillsByTheLargestSizeSeenInThePass() {
        let model = syncing([folder("Photos", .checking(files: 3_120, bytes: 4_200_000_000))])
        #expect(model.syncHeader.meter == Meter(fill: .fraction(0), caption: .bytes(done: 0, total: 4_200_000_000)))
        model.apply(.folders(folders: [folder("Photos", .checking(files: 2_000, bytes: 3_100_000_000))]))
        model.apply(.progress(progress: transfers(up: 214, done: 0, total: 50_000_000, current: "Reports/q3.pdf")))
        let header = model.syncHeader
        #expect(header.activity == .comparing(folder: "Photos", files: 2_000))
        #expect(header.meter.caption == .bytes(done: 1_100_000_000, total: 4_200_000_000))
        #expect(header.uploads == 214)
        #expect(header.current?.path == "Reports/q3.pdf")

        model.apply(.folders(folders: [folder("Photos", .catchingUp)]))
        model.apply(.folders(folders: [folder("Photos", .checking(files: 10, bytes: 1_000))]))
        #expect(model.syncHeader.meter.caption == .bytes(done: 0, total: 1_000))
    }

    @Test func comparingBeforeTheSizeIsKnownFallsBackToTheTransfers() {
        let header = syncing([folder("Photos", .checking(files: 3_120, bytes: 0))], transfers(up: 2, done: 148_000_000, total: 240_000_000)).syncHeader
        #expect(header.meter == Meter(fill: .fraction(148.0 / 240), caption: .bytes(done: 148_000_000, total: 240_000_000)))
    }

    @Test func transfersAloneShowNoActivityLine() {
        let header = syncing([folder("Documents", nil)], transfers(up: 320, down: 1_204, done: 148_000_000, total: 240_000_000, current: "Reports/q3-draft.pdf")).syncHeader
        #expect(header.activity == nil)
        #expect(header.note == nil)
        #expect(header.meter.caption == .bytes(done: 148_000_000, total: 240_000_000))
        #expect(header.meter.backlog == nil)
        #expect((header.uploads, header.downloads) == (320, 1_204))
    }

    @Test func fromAGigabyteLeftTheBacklogGetsItsOwnRow() {
        let below = syncing([folder("Photos", nil)], transfers(up: 1, done: 1, total: SyncHeader.backlogFrom)).syncHeader
        #expect(below.meter.backlog == nil)
        let header = syncing([folder("Photos", nil)], transfers(up: 820, down: 420, done: 2_600_000_000, total: 10_800_000_000)).syncHeader
        #expect(header.meter.caption == nil)
        #expect(header.meter.backlog == .left(bytes: 8_200_000_000, files: 1_240))
        #expect(header.meter.remaining == nil)
    }

    @Test func aScanBesideTransfersGoesUnderThem() {
        let one = syncing([folder("Photos", .scanning(files: 12_408))], transfers(up: 3, done: 1, total: 10)).syncHeader
        #expect(one.activity == nil)
        #expect(one.note == .scanning(folder: "Photos", files: 12_408))
        #expect(one.meter.caption == .bytes(done: 1, total: 10))

        let two = syncing([folder("Documents", nil), folder("Photos", .scanning(files: 12_408))], transfers(up: 320, done: 148, total: 240)).syncHeader
        #expect(two.folders == 2)
        #expect(two.activity == nil)
        #expect(two.note == .scanning(folder: "Photos", files: 12_408))
    }

    @Test func severalFoldersCountOnlyTheOnesThatMove() {
        let names = ["Documents", "Photos", "Music", "Design", "Archive"]
        let five = syncing(names.map { folder($0, nil) }, transfers(up: 320, down: 1_204, done: 2_600_000_000, total: 6_400_000_000)).syncHeader
        #expect(five.folders == 5)
        #expect(five.note == nil)

        let waiting = syncing([folder("Documents", nil), folder("Music", .applying(changes: 3)), folder("Photos", .awaitingAnswers(questions: 1))],
                              transfers(up: 320, done: 148, total: 240), scenario: .massDelete)
        #expect(waiting.headline == .status(.syncing))
        #expect(waiting.syncHeader.folders == 2)
        #expect(waiting.syncHeader.note == .waitingForAnswer(.one("Photos")))
    }

    @Test func severalFoldersWithoutTransfersStillSlide() {
        let header = syncing([folder("Documents", .listing), folder("Photos", .checking(files: 9, bytes: 900))]).syncHeader
        #expect(header.activity == nil)
        #expect(header.meter == Meter(fill: .sliding))
        #expect(header.note == .comparing(folder: "Photos", files: 9))
    }

    @Test func waitingForAnswersAloneLeadsWithTheQuestion() {
        let model = syncing([folder("Photos", .awaitingAnswers(questions: 1)), folder("Documents", nil, state: .upToDate)], scenario: .massDelete)
        #expect(model.headline == .questions)
        #expect(model.foldersWaitingForAnswers == .one("Photos"))

        let three = syncing(["Photos", "Music", "Design"].map { folder($0, .awaitingAnswers(questions: 2)) }, scenario: .massDelete)
        #expect(three.foldersWaitingForAnswers == .several(3))
    }

    @Test func aQuestionDoesNotHideAFolderThatSyncsWithoutSayingHow() {
        let model = syncing([folder("Documents", nil), folder("Photos", .awaitingAnswers(questions: 1))], scenario: .massDelete)
        #expect(model.headline == .status(.syncing))
        #expect(model.syncHeader.meter == Meter(fill: .sliding))
        #expect(model.syncHeader.note == .waitingForAnswer(.one("Photos")))
    }

    @Test func aSyncingFolderThatReportsNothingStillSlides() {
        let model = syncing([folder("Photos", nil)])
        #expect(model.headline == .status(.syncing))
        #expect(model.syncHeader.meter == Meter(fill: .sliding))
        #expect(model.syncHeader.activity == nil)
    }

    @Test func theEstimateAppearsOnceTheRateHoldsAndGoesWhenTransfersEnd() {
        var now = Date(timeIntervalSince1970: 1_000_000)
        let model = syncing([folder("Photos", nil)])
        model.clock = { now }
        for second in 0...61 {
            now = Date(timeIntervalSince1970: 1_000_000 + Double(second))
            model.apply(.folders(folders: [folder("Photos", nil, moved: UInt64(second) * 5_000_000)]))
            model.apply(.progress(progress: transfers(up: 1_240, done: 0, total: 8_200_000_000)))
        }
        #expect(model.remainingTime == .minutes(25))
        #expect(model.syncHeader.meter.remaining == .minutes(25))
        model.apply(.progress(progress: nil))
        #expect(model.remainingTime == nil)
    }
}

@Suite struct TransferEstimatorTests {
    private static let start = Date(timeIntervalSince1970: 1_000_000)

    private func run(_ estimator: inout TransferEstimator, seconds: ClosedRange<Int>, rate: (Int) -> UInt64,
                     left: UInt64, total: UInt64? = nil, movedBase: UInt64 = 0) -> RemainingTime? {
        var shown: RemainingTime?
        var moved = movedBase
        for second in seconds {
            moved += rate(second)
            shown = estimator.update(at: Self.start.addingTimeInterval(Double(second)), moved: moved, done: 0, total: total ?? left)
        }
        return shown
    }

    @Test func noEstimateBeforeAMinuteOfSamples() {
        var estimator = TransferEstimator()
        #expect(run(&estimator, seconds: 0...59, rate: { _ in 1_000_000 }, left: 3_000_000_000) == nil)
        #expect(run(&estimator, seconds: 60...60, rate: { _ in 1_000_000 }, left: 3_000_000_000, movedBase: 60_000_000) == .minutes(50))
    }

    @Test func anUnsteadyRateShowsNoEstimate() {
        var estimator = TransferEstimator()
        #expect(run(&estimator, seconds: 0...90, rate: { $0 < 45 ? 1_000_000 : 2_000_000 }, left: 3_000_000_000) == nil)
    }

    @Test func aStalledTransferHidesTheEstimate() {
        var estimator = TransferEstimator()
        #expect(run(&estimator, seconds: 0...60, rate: { _ in 1_000_000 }, left: 3_000_000_000) != nil)
        #expect(run(&estimator, seconds: 61...70, rate: { _ in 0 }, left: 3_000_000_000, movedBase: 61_000_000) == nil)
    }

    @Test func aMinuteOrLessShowsNoEstimate() {
        var estimator = TransferEstimator()
        #expect(run(&estimator, seconds: 0...60, rate: { _ in 1_000_000 }, left: 60_000_000) == nil)
    }

    @Test func roundingFollowsTheSize() {
        let cases: [(UInt64, RemainingTime)] = [
            (130, .minutes(2)), (280, .minutes(5)), (1_510, .minutes(25)), (3_500, .hours(1)),
            (5_300, .hours(1)), (5_400, .hours(2)), (8_000, .hours(2)), (9_100, .hours(3)),
        ]
        for (seconds, expected) in cases {
            var estimator = TransferEstimator()
            #expect(run(&estimator, seconds: 0...60, rate: { _ in 1_000_000 }, left: seconds * 1_000_000) == expected, "\(seconds) s")
        }
    }

    @Test func aRestartNeedsThirtySecondsOfNewSamples() {
        var estimator = TransferEstimator()
        #expect(run(&estimator, seconds: 0...60, rate: { _ in 1_000_000 }, left: 1_500_000_000) == .minutes(25))
        var moved: UInt64 = 0
        var shown: [RemainingTime?] = []
        for second in 61...100 {
            moved += 1_000_000
            shown.append(estimator.update(at: Self.start.addingTimeInterval(Double(second)), moved: moved, done: 0, total: 1_500_000_000))
        }
        #expect(shown[0] == nil)
        #expect(shown[28] == nil)
        #expect(shown[30] == .minutes(25))
    }

    @Test func aGrowingBacklogHidesTheEstimateUntilTheRateHoldsAgain() {
        var estimator = TransferEstimator()
        #expect(run(&estimator, seconds: 0...60, rate: { _ in 1_000_000 }, left: 1_500_000_000) == .minutes(25))
        #expect(run(&estimator, seconds: 61...61, rate: { _ in 1_000_000 }, left: 1_700_000_000, movedBase: 60_000_000) == nil)
        #expect(run(&estimator, seconds: 62...91, rate: { _ in 1_000_000 }, left: 1_700_000_000, movedBase: 61_000_000) == .minutes(30))
    }

    @Test func theEstimateStepsUpOnlyAfterTheHigherValueHolds() {
        var estimator = TransferEstimator()
        #expect(run(&estimator, seconds: 0...60, rate: { _ in 1_000_000 }, left: 1_500_000_000) == .minutes(25))
        var moved: UInt64 = 60_000_000
        var shown: [RemainingTime?] = []
        for second in 61...100 {
            moved += 1_000_000
            shown.append(estimator.update(at: Self.start.addingTimeInterval(Double(second)), moved: moved, done: 0, total: 1_650_000_000))
        }
        #expect(shown[0] == .minutes(25))
        #expect(shown[29] == .minutes(25))
        #expect(shown[30] == .minutes(30))
    }
}
