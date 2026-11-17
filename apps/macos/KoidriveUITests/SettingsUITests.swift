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
    private func assertToolbarShowsEveryTab(scenario: String) {
        let app = XCUIApplication()
        app.launchArguments = ["-FakeScenario", scenario, "-settingsTab", "storage", "-OpenSharedLinks", "YES"]
        app.launch()
        addTeardownBlock { app.terminate() }
        XCTAssert(app.windows["Shared Links"].waitForExistence(timeout: 20), "no Shared Links window in \(scenario)")
        app.typeKey(",", modifierFlags: .command)
        let window = app.windows["Storage"]
        XCTAssert(window.waitForExistence(timeout: 20), "Settings did not open in \(scenario); windows: \(app.windows.allElementsBoundByIndex.map(\.title))")
        let toolbar = window.toolbars.firstMatch
        XCTAssert(toolbar.waitForExistence(timeout: 10), "no toolbar in \(scenario)")

        let titles = ["Folders", "Account", "Storage", "Problems", "General"]
        XCTAssertEqual(toolbar.buttons.allElementsBoundByIndex.map(\.label), titles, scenario)
        let frames = titles.map { toolbar.buttons[$0].frame }
        for (title, frame) in zip(titles, frames) {
            XCTAssertGreaterThan(frame.width * frame.height, 0, "\(title) has no size in \(scenario)")
        }
        let gaps = zip(frames, frames.dropFirst()).map { $1.minX - $0.maxX }
        XCTAssertLessThan(gaps.max()! - gaps.min()!, 2, "uneven toolbar gaps \(gaps) in \(scenario)")
    }
}
