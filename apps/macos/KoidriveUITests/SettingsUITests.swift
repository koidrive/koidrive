// SPDX-License-Identifier: Apache-2.0

import XCTest

final class SettingsUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    @MainActor
    func testTheToolbarShowsEveryTabWithAProblem() {
        assertToolbarShowsEveryTab(scenario: "problems")
    }

    @MainActor
    func testTheToolbarShowsEveryTabWithoutProblems() {
        assertToolbarShowsEveryTab(scenario: "upToDate")
    }

    @MainActor
    private func assertToolbarShowsEveryTab(scenario: String, file: StaticString = #filePath, line: UInt = #line) {
        let app = XCUIApplication()
        app.launchArguments = ["-FakeScenario", scenario, "-settingsTab", "storage"]
        app.launch()
        addTeardownBlock { app.terminate() }
        app.menuBarItems["Koidrive"].menuItems["Settings…"].click()
        let window = app.windows["Storage"]
        XCTAssert(window.waitForExistence(timeout: 20), file: file, line: line)
        let toolbar = window.toolbars.firstMatch
        XCTAssert(toolbar.waitForExistence(timeout: 10), file: file, line: line)

        let titles = ["Folders", "Account", "Storage", "Problems", "General"]
        XCTAssertEqual(toolbar.buttons.allElementsBoundByIndex.map(\.label), titles, file: file, line: line)
        let frames = titles.map { toolbar.buttons[$0].frame }
        for (title, frame) in zip(titles, frames) {
            XCTAssertGreaterThan(frame.width * frame.height, 0, "\(title) has no size", file: file, line: line)
        }
        let gaps = zip(frames, frames.dropFirst()).map { $1.minX - $0.maxX }
        XCTAssertLessThan(gaps.max()! - gaps.min()!, 2, "uneven toolbar gaps \(gaps)", file: file, line: line)

        toolbar.buttons["Problems"].click()
        XCTAssert(app.windows["Problems"].waitForExistence(timeout: 10), file: file, line: line)
    }
}
