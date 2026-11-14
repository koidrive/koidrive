// SPDX-License-Identifier: Apache-2.0

import XCTest

final class ShareLinksUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    @MainActor
    func testAFileIsSharedAndItsLinkRevoked() {
        let app = XCUIApplication()
        app.launchArguments = ["-FakeScenario", "upToDate", "-OpenShareLink", "Documents/Reports/Annual Report.docx"]
        app.launch()
        addTeardownBlock { app.terminate() }

        let share = app.windows["Share Link"]
        XCTAssert(share.waitForExistence(timeout: 20))
        XCTAssert(text("Anyone with the link can download this file.", in: share).waitForExistence(timeout: 10))
        share.buttons["Create Link"].click()

        let url = share.staticTexts["share-url"]
        XCTAssert(url.waitForExistence(timeout: 10))
        XCTAssert((url.value as? String ?? url.label).hasPrefix("box.example.com/s/"))
        XCTAssert(share.buttons["Copy"].exists)
        XCTAssert(text("Copy it now. Koidrive won’t show this link again.", in: share).exists)

        share.buttons["Shared Links…"].click()
        let list = app.windows["Shared Links"]
        XCTAssert(list.waitForExistence(timeout: 10))
        let revoke = list.buttons["Revoke…"].firstMatch
        XCTAssert(revoke.waitForExistence(timeout: 20))
        XCTAssertFalse(text("Revoked", in: list).exists)
        revoke.click()

        let confirm = list.sheets.firstMatch.buttons["Revoke"]
        XCTAssert(confirm.waitForExistence(timeout: 10))
        confirm.click()
        XCTAssert(text("Revoked", in: list).waitForExistence(timeout: 10))
    }

    @MainActor
    private func text(_ value: String, in window: XCUIElement) -> XCUIElement {
        window.staticTexts.matching(NSPredicate(format: "value == %@ OR label == %@", value, value)).firstMatch
    }
}
