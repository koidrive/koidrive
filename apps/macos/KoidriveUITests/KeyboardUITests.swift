// SPDX-License-Identifier: Apache-2.0

import XCTest

final class KeyboardUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    @MainActor
    func testAConflictIsResolvedFromTheKeyboard() {
        let app = launch("conflict")
        let window = app.windows["Needs You"]
        XCTAssert(window.waitForExistence(timeout: 20))
        let keepCopy = window.buttons["Keep Copy"]
        XCTAssert(keepCopy.waitForExistence(timeout: 20))
        window.staticTexts["Versions"].click()

        tab(until: keepCopy, in: app)
        app.typeKey(" ", modifierFlags: [])
        let settled = window.staticTexts.matching(NSPredicate(format: "value CONTAINS %@ OR label CONTAINS %@", "now has the copy", "now has the copy")).firstMatch
        XCTAssert(settled.waitForExistence(timeout: 10))
    }

    @MainActor
    func testEveryAccountActionIsReachedFromTheKeyboard() {
        let app = launch("conflict", "-settingsTab", "account")
        XCTAssert(app.windows["Needs You"].waitForExistence(timeout: 20))
        app.typeKey(",", modifierFlags: .command)
        let window = app.windows["Account"]
        XCTAssert(window.waitForExistence(timeout: 20))
        let remove = window.buttons["Remove…"]
        XCTAssert(remove.waitForExistence(timeout: 20))
        window.staticTexts["Server"].click()

        tab(until: window.buttons["Sign Out…"], in: app)
        tab(until: remove, in: app)
        let add = window.buttons["Add a Device…"]
        tab(until: add, in: app)
        app.typeKey(" ", modifierFlags: [])
        let code = window.staticTexts.matching(NSPredicate(format: "value == %@ OR label == %@", "Pairing code", "Pairing code")).firstMatch
        XCTAssert(code.waitForExistence(timeout: 10))
    }

    @MainActor
    private func launch(_ scenario: String, _ arguments: String...) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-FakeScenario", scenario, "-AppleKeyboardUIMode", "2"] + arguments
        app.launch()
        addTeardownBlock { app.terminate() }
        return app
    }

    @MainActor
    private func tab(until element: XCUIElement, in app: XCUIApplication, file: StaticString = #filePath, line: UInt = #line) {
        var visited = [describeFocus(in: app)]
        for _ in 0..<30 where !focused(element) {
            app.typeKey(.tab, modifierFlags: [])
            visited.append(describeFocus(in: app))
        }
        XCTAssert(focused(element), "Tab never reached \(element). Focus went through: \(visited)", file: file, line: line)
    }

    @MainActor
    private func describeFocus(in app: XCUIApplication) -> String {
        let current = app.descendants(matching: .any).matching(NSPredicate(format: "hasKeyboardFocus == true")).firstMatch
        return current.exists ? "\(current.elementType.rawValue):\(current.label):\(current.identifier)" : "none"
    }

    @MainActor
    private func focused(_ element: XCUIElement) -> Bool {
        element.value(forKey: "hasKeyboardFocus") as? Bool == true
    }
}
