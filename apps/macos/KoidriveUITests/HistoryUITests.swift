// SPDX-License-Identifier: Apache-2.0

import XCTest

final class HistoryUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    @MainActor
    func testADeletedFileIsRestored() {
        let app = launch("-OpenDeletedFiles", "YES")
        let window = app.windows["Deleted Files"]
        XCTAssert(window.waitForExistence(timeout: 20))

        let budget = text("Q3 Budget.xlsx", in: window)
        XCTAssert(budget.waitForExistence(timeout: 20))
        budget.click()
        XCTAssert(text("Restores to", in: window).waitForExistence(timeout: 5))
        window.buttons["Restore"].click()

        XCTAssert(text("Restore requested", in: window).waitForExistence(timeout: 10))
        window.buttons["Show Restores"].click()
        XCTAssert(text("Waiting for Documents to sync", in: window).waitForExistence(timeout: 10))
    }

    @MainActor
    func testAnEarlierVersionIsRestored() {
        let app = launch("-OpenVersionHistory", "Documents/Reports/Annual Report.docx")
        let window = app.windows["Version History"]
        XCTAssert(window.waitForExistence(timeout: 20))

        let earlier = text("Ada's MacBook Air", in: window)
        XCTAssert(earlier.waitForExistence(timeout: 20))
        earlier.click()
        window.buttons["Restore Version"].click()

        XCTAssert(text("Restore requested", in: window).waitForExistence(timeout: 10))
        XCTAssert(text("Restore requested. It goes out with Documents’ next sync.", in: window).exists)
    }

    @MainActor
    private func launch(_ arguments: String...) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-FakeScenario", "upToDate"] + arguments
        app.launch()
        addTeardownBlock { app.terminate() }
        return app
    }

    @MainActor
    private func text(_ value: String, in window: XCUIElement) -> XCUIElement {
        window.staticTexts.matching(NSPredicate(format: "value == %@ OR label == %@", value, value)).firstMatch
    }
}
