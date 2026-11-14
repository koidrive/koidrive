// SPDX-License-Identifier: Apache-2.0

import Foundation
import KoidriveFFI
import Testing

import KoidriveModel

private let utc: Calendar = {
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = TimeZone(identifier: "UTC")!
    return calendar
}()

private let now = Date(timeIntervalSince1970: 1_794_537_000)
private let nowMs = Int64(now.timeIntervalSince1970 * 1000)
private let hourMs: Int64 = 3_600_000
private let dayMs: Int64 = 86_400_000
private let fixedClock = HistoryClock(now: { now }, calendar: utc)

@MainActor
private func fakeCore(_ model: AppModel) throws -> FakeCore {
    try #require(model.session.fakeCore())
}

@MainActor
private func folder(_ model: AppModel, _ name: String) throws -> LinkedFolder {
    try #require(model.state.folders.first { $0.spaceName == name })
}

@MainActor
private func report(in app: AppModel) throws -> URL {
    try folder(app, "Documents").url.appending(path: "Reports/Annual Report.docx")
}

@MainActor
@Suite struct ShareLinkModelTests {
    @Test func theChosenExpiryAndPasswordReachTheLink() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let file = try report(in: app)
        let share = ShareLinkModel(app: app, item: file)
        share.expiry = .month
        share.requiresPassword = true
        share.password = "correct horse"
        await share.create()

        let made = try #require(share.link)
        #expect(made.url.hasPrefix("https://box.example.com/s/"))
        #expect(made.link.itemURL == file)
        #expect(made.link.hasPassword)
        #expect(try #require(made.link.expiresAtMs) - made.link.createdAtMs == 30 * dayMs)
    }

    @Test func aLinkWorksForSevenDaysWithoutAPasswordUnlessAsked() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let share = ShareLinkModel(app: app, item: try report(in: app))
        #expect(share.expiry == .week)
        share.password = "short"
        await share.create()

        let made = try #require(share.link)
        #expect(!made.link.hasPassword)
        #expect(try #require(made.link.expiresAtMs) - made.link.createdAtMs == 7 * dayMs)
    }

    @Test func aPasswordOutsideEightTo256CharactersIsRefusedBeforeTheServerIsAsked() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let before = try await app.shareLinks().count
        let share = ShareLinkModel(app: app, item: try report(in: app))
        share.requiresPassword = true

        share.password = "seven77"
        await share.create()
        #expect(share.passwordIssue == .tooShort)
        #expect(share.step == .choosing)

        share.password = "seven777"
        #expect(share.passwordIssue == nil)
        share.password = String(repeating: "a", count: 257)
        await share.create()
        #expect(share.passwordIssue == .tooLong)
        #expect(try await app.shareLinks().count == before)

        share.password = String(repeating: "\u{E9}", count: 256)
        await share.create()
        #expect(share.passwordIssue == nil)
        #expect(share.link?.link.hasPassword == true)
    }

    @Test func lineBreaksStayOutOfThePassword() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let share = ShareLinkModel(app: app, item: try report(in: app))
        share.requiresPassword = true
        share.password = "eight\n888\r\n"
        #expect(share.password == "eight888")
        await share.create()
        #expect(share.link?.link.hasPassword == true)
    }

    @Test func theWindowChecksTheFilesThenKeepsTheURLInItsOwnModelOnly() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let file = try report(in: app)
        let share = ShareLinkModel(app: app, item: file)
        let creating = Task { await share.create() }
        await Task.yield()
        #expect(share.step == .checking)
        #expect(!share.canCreate)
        await creating.value

        let made = try #require(share.link)
        #expect(!share.canCreate)
        #expect(ShareLinkModel(app: app, item: file).link == nil)
        let links = SharedLinksModel(app: app, clock: fixedClock)
        await links.load()
        #expect(links.activeLinks.first?.link == made.link)
    }

    @Test func aFolderIsSharedAsAFolder() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let seoul = try folder(app, "Photos").url.appending(path: "2026/Seoul", directoryHint: .isDirectory)
        let share = ShareLinkModel(app: app, item: seoul)
        #expect(share.isFolder)
        #expect(share.name == "Seoul")
        await share.create()
        #expect(share.link?.link.kind == .folder)
    }

    @Test func anItemTheServerDoesNotHaveIsNotSyncedYetUntilTheStatusChanges() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let share = ShareLinkModel(app: app, item: try report(in: app))
        try fakeCore(app).failNext(error: .NotOnServer)
        await share.create()
        #expect(share.step == .notSyncedYet)
        #expect(!share.canCreate)

        share.sessionStatusChanged()
        #expect(share.canCreate)
        await share.create()
        #expect(share.link != nil)
    }

    @Test func aServerFailureCanBeTriedAgain() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let share = ShareLinkModel(app: app, item: try report(in: app))
        try fakeCore(app).failNext(error: .ServerUnreachable(reason: "timed out"))
        await share.create()
        #expect(share.step == .failed(.ServerUnreachable(reason: "timed out")))
        #expect(!share.isOffline)
        #expect(share.canCreate)

        await share.create()
        #expect(share.link != nil)
    }

    @Test func offlineALinkWaitsForTheConnection() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        let share = ShareLinkModel(app: app, item: try report(in: app))
        app.apply(.status(status: .offline))
        #expect(share.isOffline)
        #expect(!share.canCreate)
        await share.create()
        #expect(share.step == .choosing)

        app.apply(.status(status: .idle))
        #expect(!share.isOffline)
        await share.create()
        #expect(share.link != nil)
    }
}

@MainActor
@Suite struct SharedLinksModelTests {
    private func seeded() throws -> AppModel {
        let app = AppModel.fake(scenario: .upToDate)
        let documents = try folder(app, "Documents").path
        let photos = try folder(app, "Photos").path
        func link(_ id: String, _ folder: String?, _ path: String?, _ kind: NodeKind?, created: Int64, expires: Int64?,
                  password: Bool = false, _ state: ShareState = .active) -> ShareLink {
            ShareLink(id: id, folder: folder, path: path, kind: kind, createdAtMs: nowMs + created,
                      expiresAtMs: expires.map { nowMs + $0 }, hasPassword: password, state: state)
        }
        try fakeCore(app).setShareLinks(links: [
            link("link-report", documents, "Reports/Annual Report.docx", .file, created: -2 * hourMs, expires: 7 * dayMs - 2 * hourMs,
                 password: true),
            link("link-hours", documents, "Notes.md", .file, created: -19 * hourMs, expires: 5 * hourMs),
            link("link-seoul", photos, "2026/Seoul", .folder, created: -3 * dayMs, expires: 27 * dayMs),
            link("link-gone", documents, nil, nil, created: -5 * dayMs, expires: 2 * dayMs),
            link("link-cli", photos, "", .folder, created: -8 * dayMs, expires: nil),
            link("link-minutes", documents, "Reports/Board Minutes.docx", .file, created: -10 * dayMs, expires: -3 * dayMs,
                 password: true, .expired),
            link("link-old", documents, "Reports/Annual Report.docx", .file, created: -20 * dayMs, expires: 10 * dayMs, .revoked),
        ])
        return app
    }

    @Test func activeLinksShowNewestFirstAndEndedLinksOnlyWhenAsked() async throws {
        let links = SharedLinksModel(app: try seeded(), clock: fixedClock)
        await links.load()
        #expect(links.activeLinks.map(\.id) == ["link-report", "link-hours", "link-seoul", "link-gone", "link-cli"])
        #expect(links.endedLinks.isEmpty)

        links.showsEndedLinks = true
        #expect(links.endedLinks.map(\.id) == ["link-minutes", "link-old"])
        #expect(links.activeLinks.count == 5)
    }

    @Test func eachLinkNamesItsItemByItsFolder() async throws {
        let links = SharedLinksModel(app: try seeded(), clock: fixedClock)
        await links.load()
        let items = Dictionary(uniqueKeysWithValues: links.activeLinks.map { ($0.id, $0.item) })
        #expect(items["link-report"] == .present(path: "Documents/Reports/Annual Report.docx", kind: .file))
        #expect(items["link-seoul"] == .present(path: "Photos/2026/Seoul", kind: .folder))
        #expect(items["link-cli"] == .present(path: "Photos", kind: .folder))
        #expect(items["link-gone"] == .gone(folder: "Documents"))
    }

    @Test func createdAndExpiryFollowTheClock() async throws {
        let links = SharedLinksModel(app: try seeded(), clock: fixedClock)
        links.showsEndedLinks = true
        await links.load()
        let shown = Dictionary(uniqueKeysWithValues: (links.activeLinks + links.endedLinks).map { ($0.id, $0) })
        #expect(shown["link-report"]?.createdDay == .today)
        #expect(shown["link-hours"]?.createdDay == .yesterday)
        #expect(shown["link-seoul"]?.createdDay == .earlier)
        #expect(shown["link-report"]?.end == .remaining(.days(6)))
        #expect(shown["link-hours"]?.end == .remaining(.hours(5)))
        #expect(shown["link-seoul"]?.end == .remaining(.days(27)))
        #expect(shown["link-cli"]?.end == LinkEnd.never)
        #expect(shown["link-minutes"]?.end == .on(Date(timeIntervalSince1970: TimeInterval(nowMs - 3 * dayMs) / 1000)))
    }

    @Test func aRevokedLinkStaysInItsRowAndShowsItEnded() async throws {
        let app = try seeded()
        let links = SharedLinksModel(app: app, clock: fixedClock)
        await links.load()
        let report = try #require(links.activeLinks.first)
        await links.revoke(report.link)

        let row = try #require(links.activeLinks.first)
        #expect(row.id == report.id)
        #expect(row.link.state == .revoked)
        #expect(row.end == .on(Date(timeIntervalSince1970: TimeInterval(nowMs + 7 * dayMs - 2 * hourMs) / 1000)))
        #expect(links.revokeError == nil)

        await links.sessionStatusChanged()
        #expect(links.activeLinks.first?.link.state == .revoked)
        links.showsEndedLinks = true
        #expect(!links.endedLinks.contains { $0.id == report.id })
        #expect(try await app.shareLinks().first { $0.id == report.id }?.state == .revoked)
    }

    @Test func aFailedRevokeKeepsTheLinkActiveAndSaysWhy() async throws {
        let app = try seeded()
        let links = SharedLinksModel(app: app, clock: fixedClock)
        await links.load()
        let report = try #require(links.activeLinks.first)
        try fakeCore(app).failNext(error: .ShareLinkNotFound)
        await links.revoke(report.link)

        #expect(links.revokeError == .ShareLinkNotFound)
        #expect(links.revoking == nil)
        #expect(links.activeLinks.first?.link.state == .active)
    }

    @Test func noLinksIsAnEmptyList() async throws {
        let app = AppModel.fake(scenario: .upToDate)
        try fakeCore(app).setShareLinks(links: [])
        let links = SharedLinksModel(app: app, clock: fixedClock)
        await links.load()
        #expect(links.content == .loaded([]))
        #expect(links.activeLinks.isEmpty)
    }

    @Test func aFailedLoadIsAnErrorOrOfflineAndLoadsAgainWhenTheStatusChanges() async throws {
        let app = try seeded()
        let links = SharedLinksModel(app: app, clock: fixedClock)
        try fakeCore(app).failNext(error: .ServerUnreachable(reason: "timed out"))
        await links.load()
        #expect(links.content == .failed(.ServerUnreachable(reason: "timed out")))
        #expect(!links.isOffline)

        app.apply(.status(status: .offline))
        #expect(links.isOffline)
        app.apply(.status(status: .idle))
        await links.sessionStatusChanged()
        #expect(links.activeLinks.count == 5)
    }

    @Test func onlyItemsInALinkedFolderCanBeShared() throws {
        let app = AppModel.fake(scenario: .upToDate)
        let links = SharedLinksModel(app: app, clock: fixedClock)
        #expect(links.canShare(try report(in: app)))
        #expect(links.canShare(try folder(app, "Photos").url))
        #expect(!links.canShare(FileManager.default.temporaryDirectory.appending(path: "Elsewhere/a.docx")))
    }
}
