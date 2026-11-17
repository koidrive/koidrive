// SPDX-License-Identifier: Apache-2.0

import XCTest

final class GitignoreSettingUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    @MainActor
    func testTurningOffAsksFirstAndTurningOnSaysWhatStoppedSyncing() {
        let app = XCUIApplication()
        app.launchArguments = ["-FakeScenario", "upToDate", "-OpenDeletedFiles", "YES", "-settingsTab", "advanced"]
        app.launch()
        addTeardownBlock { app.terminate() }
        XCTAssert(app.windows["Deleted Files"].waitForExistence(timeout: 20))
        app.typeKey(",", modifierFlags: .command)
        let window = app.windows["Advanced"]
        XCTAssert(window.waitForExistence(timeout: 20))
        let follow = window.checkBoxes["Follow .gitignore files"]
        XCTAssert(follow.waitForExistence(timeout: 10))
        XCTAssertEqual(isOn(follow), true)

        follow.click()
        let syncFiles = app.buttons["Sync Files"]
        XCTAssert(syncFiles.waitForExistence(timeout: 10))
        XCTAssert(text("Sync ignored files?", in: app).exists)
        XCTAssert(text("55,026 files (1.24 GB) in Documents and Photos will upload.", in: app).waitForExistence(timeout: 10))
        app.buttons["Cancel"].click()
        XCTAssert(syncFiles.waitForNonExistence(timeout: 10))
        XCTAssertEqual(isOn(follow), true)

        follow.click()
        XCTAssert(syncFiles.waitForExistence(timeout: 10))
        syncFiles.click()
        XCTAssert(syncFiles.waitForNonExistence(timeout: 10))
        XCTAssert(wait(for: follow, on: false))

        follow.click()
        let stopped = text("55,026 files no longer sync from this Mac. They stay on this Mac and on the server.", in: window)
        XCTAssert(stopped.waitForExistence(timeout: 10))
        XCTAssertFalse(syncFiles.exists)
        XCTAssertEqual(isOn(follow), true)
    }

    @MainActor
    private func isOn(_ toggle: XCUIElement) -> Bool? {
        switch toggle.value {
        case let value as Int: value == 1
        case let value as String: value == "1"
        case let value as Bool: value
        default: nil
        }
    }

    @MainActor
    private func wait(for toggle: XCUIElement, on: Bool) -> Bool {
        let until = Date.now.addingTimeInterval(10)
        while Date.now < until {
            if isOn(toggle) == on { return true }
            RunLoop.current.run(until: .now.addingTimeInterval(0.1))
        }
        return false
    }

    @MainActor
    private func text(_ value: String, in element: XCUIElement) -> XCUIElement {
        element.staticTexts.matching(NSPredicate(format: "value == %@ OR label == %@", value, value)).firstMatch
    }
}
